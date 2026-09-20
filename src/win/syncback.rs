//! 本地变更回传(M4):watcher + 防抖队列 + worker。
//!
//! 事件源双通道:
//! - cldapi 回调(provider.rs):closed(写句柄关闭)/delete/rename —— 占位符
//!   专属。delete/rename 在回调内同步打 API(秒级),失败返回 Err 阻止本地
//!   操作(Explorer 会把文件弹回来,忠实反馈)
//! - 自建 ReadDirectoryChangesW watcher:本地新建普通文件/目录、普通文件
//!   改名/删除没有 cldapi 回调,只能靠它。**不订阅 ATTRIB**(pin/unpin 由
//!   crate 自带的属性 watcher 供给 sync_pin_state,别混流)
//!
//! 回环抑制不靠计时:worker 落盘前重新分类——占位符且 in_sync(枚举建出的
//! 新占位符、水合写数据都属此类)直接丢;占位符且 !in_sync 才是用户改动。
//! 平台按注册的 InSyncPolicy 在真实写入时自动清 in_sync,这就是脏信号。

use crate::baidu::NetFile;
use crate::core::{self, DirCache, SharedClient};
use crate::progress::Progress;
use cloud_filter::placeholder::Placeholder;
use std::collections::{HashMap, HashSet};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// 防抖:同一文件连续改动,静默 2s 才动手
const DEBOUNCE: Duration = Duration::from_secs(2);
/// 重试退避(指数间隔上限 10m;5 次后放弃,留脏等下次启动脏扫)
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
    Duration::from_secs(3600),
];

/// watcher → worker 的事件
enum Cmd {
    /// 内容可能变了(写完关句柄 / LAST_WRITE / SIZE)
    Touch(PathBuf),
    /// 新条目出现(本地新建/拖入;也可能是我们枚举建的占位符——落盘时再分)
    Added(PathBuf),
    /// 改名/移动(普通文件才有;占位符的改名走 cldapi rename 回调)
    Renamed(PathBuf, PathBuf),
    /// 删除(占位符的删除走 cldapi delete 回调,watcher 侧被 handled 抵掉)
    Removed(PathBuf),
}

/// worker 里的待办(按本地路径为键,后到覆盖先到)
struct Pending {
    job: Job,
    due: Instant,
    tries: u8,
}

#[derive(Clone)]
enum Job {
    /// 占位符被改过 → 上传 + 刷 blob + mark_in_sync
    Upload,
    /// 本地新条目(文件上传/目录 mkdir)→ 转占位符
    NewEntry,
    /// 普通文件改名 → 远端 mv
    Move { from: PathBuf },
    /// 普通条目删除 → 远端 delete
    Delete,
}

/// 回传协调器:provider 持有,回调里只投事件(回调内绝不做长操作)
pub(crate) struct SyncBack {
    tx: Sender<Cmd>,
    /// cldapi 回调已处理过的路径(delete/rename 成功后登记,与 worker 共享),
    /// 用来抵掉随后必然到来的 watcher 事件(Removed/Renamed/改名后补的 Touch)
    handled: Arc<Mutex<HashSet<PathBuf>>>,
}

impl SyncBack {
    pub fn new(
        client: Arc<SharedClient>,
        dir_cache: Arc<DirCache>,
        progress: Progress,
        sync_root: PathBuf,
        root: String,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_watcher(sync_root.clone(), tx.clone());
        let worker_tx = tx.clone();
        let handled = Arc::new(Mutex::new(HashSet::new()));
        let worker_handled = handled.clone();
        std::thread::Builder::new()
            .name("syncback".into())
            .spawn(move || {
                worker(
                    rx,
                    worker_tx,
                    worker_handled,
                    client,
                    dir_cache,
                    progress,
                    sync_root,
                    root,
                )
            })
            .expect("起 syncback worker 线程");
        Self { tx, handled }
    }

    /// 写句柄关了/文件被改:投给 worker 分类(占位符且脏才传)
    pub(crate) fn touch(&self, p: PathBuf) {
        let _ = self.tx.send(Cmd::Touch(p));
    }

    /// 本地新条目(拖入/新建;也用于 rename 回调里"外部移入"的情形)
    pub(crate) fn new_entry(&self, p: PathBuf) {
        let _ = self.tx.send(Cmd::Added(p));
    }

    /// cldapi delete/rename 回调成功后登记,抵掉 watcher 的重复事件
    pub(crate) fn mark_handled(&self, p: PathBuf) {
        self.handled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(p);
    }
    /// 启动脏扫(异步,须在 provider 连接成功后才真正动工——worker 的
    /// update/convert 是 provider-only 调用;连接本身毫秒级,睡 1s 兜底)。
    /// 递归找脏文件/漏网普通条目,纯本地判断不打 API
    pub(crate) fn scan_after_connect(&self, root: PathBuf) {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("dirty-scan".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                walk_dirty(&root, &root, 0, &tx);
                tracing::info!("启动脏扫完成");
            })
            .expect("起脏扫线程");
    }
}

/// 递归找脏文件/漏网普通条目(纯本地判断,不打 API)
fn walk_dirty(dir: &Path, stop: &Path, depth: usize, tx: &Sender<Cmd>) {
    if depth > 16 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        let is_ph = md.file_attributes() & 0x400 != 0;
        if md.is_dir() {
            walk_dirty(&p, stop, depth + 1, tx);
            if !is_ph {
                // 撕裂目录由启动扫修;这里只兜"用户本地新建的目录"
                // 两者本地形态相同(普通目录),repair_torn_dirs 先跑,
                // 转成占位符后这里 NewEntry 落盘时按"已是占位符"丢弃
                let _ = tx.send(Cmd::Added(p));
            }
        } else if is_ph {
            let dirty = Placeholder::open(&p)
                .and_then(|ph| ph.info().map_err(std::convert::Into::into))
                .ok()
                .flatten()
                .map_or(true, |i| !i.is_in_sync());
            if dirty {
                let _ = tx.send(Cmd::Touch(p));
            }
        } else {
            let _ = tx.send(Cmd::Added(p));
        }
    }
    let _ = stop; // 预留:以后按 sync_root 做相对化
}

// ---------- worker ----------

struct Ctx {
    tx: Sender<Cmd>,
    handled: Arc<Mutex<HashSet<PathBuf>>>,
    client: Arc<SharedClient>,
    dir_cache: Arc<DirCache>,
    progress: Progress,
    sync_root: PathBuf,
    root: String,
}

/// handled 命中即取走(一次性:只抵平台补发的那串事件,用户随后的真改动不受影响)
fn take_handled(p: &Path, ctx: &Ctx) -> bool {
    ctx.handled
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(p)
}

fn worker(
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    handled: Arc<Mutex<HashSet<PathBuf>>>,
    client: Arc<SharedClient>,
    dir_cache: Arc<DirCache>,
    progress: Progress,
    sync_root: PathBuf,
    root: String,
) {
    let ctx = Ctx {
        tx,
        handled,
        client,
        dir_cache,
        progress,
        sync_root,
        root,
    };
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    loop {
        // 睡到最近一个到期(到期即 0s,recv 立刻醒→process_due);
        // 没有待办就长睡,事件来了 recv 会醒
        let timeout = pending
            .values()
            .map(|p| p.due)
            .min()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(cmd) => fold(cmd, &mut pending, &ctx),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("syncback worker 退出(事件通道关闭)");
                return;
            }
        }
        process_due(&mut pending, &ctx);
    }
}

/// 事件入队/合并。分类放在这里(事件刚发生,文件状态新鲜):
/// 占位符且 in_sync → 噪音,丢;占位符且脏 → Upload;普通条目 → NewEntry。
/// handled 命中也要 take:cldapi 的 rename/delete 回调办完远端后,平台
/// 还会补一串本地事件(改名清 in_sync 触发 Touch 等),一并吃掉
fn fold(cmd: Cmd, pending: &mut HashMap<PathBuf, Pending>, ctx: &Ctx) {
    match cmd {
        Cmd::Touch(p) | Cmd::Added(p) => {
            if take_handled(&p, ctx) {
                return;
            }
            match classify(&p, ctx) {
                Some(cls) => {
                    // 同路径已排队 → 只续防抖;没有 → 新建(Added 的普通条目
                    // 和 Touch 的脏占位符共用键,后到覆盖)
                    let entry = pending.entry(p).or_insert(Pending {
                        job: cls.clone(),
                        due: Instant::now(),
                        tries: 0,
                    });
                    entry.job = cls;
                    entry.due = Instant::now() + DEBOUNCE;
                }
                None => {
                    // 变回干净(比如我们自己的 mark_in_sync 抢在事件前生效):
                    // 若之前排了队,撤掉
                    pending.remove(&p);
                }
            }
        }
        Cmd::Removed(p) => {
            if take_handled(&p, ctx) {
                return;
            }
            pending.remove(&p);
            pending.insert(
                p.clone(),
                Pending {
                    job: Job::Delete,
                    due: Instant::now() + Duration::from_secs(1),
                    tries: 0,
                },
            );
        }
        Cmd::Renamed(from, to) => {
            if take_handled(&from, ctx) {
                return;
            }
            pending.remove(&from);
            let prev = pending.insert(
                to.clone(),
                Pending {
                    job: Job::Move { from },
                    due: Instant::now() + Duration::from_secs(1),
                    tries: 0,
                },
            );
            // to 上已排的活(比如刚新建就改名)被覆盖——旧路径的远端对象
            // 会随 Move 一起搬走,不用补
            let _ = prev;
        }
    }
}

/// None = 不是活(不存在/水合写数据/占位符且 in_sync/根目录自身)。
/// 水合在途的写事件不查占位符:一来是平台写的(in_sync 不会清),
/// 二来 classify 的 open 要抢 oplock,跟水合抢会拖慢下载
fn classify(p: &Path, ctx: &Ctx) -> Option<Job> {
    if p == ctx.sync_root || super::provider::is_hydrating(p) {
        return None;
    }
    let Ok(md) = std::fs::metadata(p) else {
        return None;
    };
    let is_ph = md.file_attributes() & 0x400 != 0;
    if is_ph {
        let in_sync = Placeholder::open(p)
            .and_then(|ph| ph.info().map_err(std::convert::Into::into))
            .ok()
            .flatten()
            .map_or(true, |i| i.is_in_sync());
        if in_sync {
            None
        } else {
            super::provider::mark_dirty(p);
            Some(Job::Upload)
        }
    } else {
        Some(Job::NewEntry)
    }
}

fn process_due(pending: &mut HashMap<PathBuf, Pending>, ctx: &Ctx) {
    let due_keys: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, p)| p.due <= Instant::now())
        .map(|(k, _)| k.clone())
        .collect();
    for key in due_keys {
        let Some(ent) = pending.get(&key) else { continue };
        // 执行前再查一次 handled:watcher 事件抢在 cldapi 回调登记之前入的队
        // (如改名:事件即时到,回调的 mv 要 2s 才回来登记),这里兜住
        if take_handled(&key, ctx) {
            tracing::debug!("回传 {} 已由回调处理,撤销", key.display());
            pending.remove(&key);
            continue;
        }
        let job = match &ent.job {
            Job::Upload => Job::Upload,
            Job::NewEntry => Job::NewEntry,
            Job::Move { from } => Job::Move { from: from.clone() },
            Job::Delete => Job::Delete,
        };
        let tries = ent.tries;
        match run_job(&key, job, ctx) {
            Ok(Outcome::Done) => {
                pending.remove(&key);
            }
            Ok(Outcome::Requeue) => {
                // 边传边写之类:稍后再来,不吃重试次数
                if let Some(e) = pending.get_mut(&key) {
                    e.due = Instant::now() + Duration::from_secs(3);
                }
            }
            Err(e) => {
                tracing::warn!("回传 {} 失败({tries} 次):{e:#}", key.display());
                if tries as usize >= BACKOFF.len() {
                    tracing::error!("回传 {} 放弃,保持脏状态等下次启动脏扫", key.display());
                    pending.remove(&key);
                } else if let Some(e) = pending.get_mut(&key) {
                    e.tries = tries + 1;
                    e.due = Instant::now() + BACKOFF[tries as usize];
                }
            }
        }
    }
}

enum Outcome {
    Done,
    /// 需要重排(文件还在被写),不算失败
    Requeue,
}

fn run_job(key: &Path, job: Job, ctx: &Ctx) -> anyhow::Result<Outcome> {
    let Some(remote) = core::local_to_remote(key, &ctx.sync_root, &ctx.root) else {
        tracing::warn!("回传路径在同步根之外:{},丢弃", key.display());
        return Ok(Outcome::Done);
    };
    match job {
        Job::Upload => upload_dirty(key, &remote, ctx),
        Job::NewEntry => new_entry(key, &remote, ctx),
        Job::Move { from } => remote_move(&from, key, &remote, ctx),
        Job::Delete => remote_delete(&remote, ctx),
    }
}

/// 改过的占位符:上传 → 刷 blob(fs_id 变了!)→ mark_in_sync。
/// 上传前快照 size/mtime,传完变了说明用户还在写 → Requeue
fn upload_dirty(local: &Path, remote: &str, ctx: &Ctx) -> anyhow::Result<Outcome> {
    use cloud_filter::placeholder::UpdateOptions;
    let md0 = std::fs::metadata(local)?;
    let new_id = core::upload_local_file(&ctx.client, &ctx.progress, local, remote, md0.len())?;
    let md1 = std::fs::metadata(local)?;
    if md1.len() != md0.len() || mtime_s(&md1) != mtime_s(&md0) {
        tracing::info!("{} 上传期间又被写,重排", local.display());
        return Ok(Outcome::Requeue);
    }
    let id = match new_id {
        Some(id) => id,
        None => resolve_remote_id(remote, ctx).unwrap_or(0), // 秒传:回查拿新 id
    };
    let blob = blob_for(id, false, md1.len(), mtime_s(&md1), remote);
    // share 7:用户多半还开着文件;独占拿不到就当失败重试(update 拒并发)
    let f = std::fs::OpenOptions::new()
        .access_mode(0x4000_0000)
        .share_mode(7)
        .open(local)?;
    let mut ph = Placeholder::from(f);
    ph.update(UpdateOptions::default().blob(&blob).mark_in_sync(), None)?;
    ctx.dir_cache.remove(&core::parent_of(remote));
    super::provider::clear_dirty(local);
    tracing::info!("已回传改动:{}({} 字节)", local.display(), md1.len());
    Ok(Outcome::Done)
}

/// 本地新条目:文件 → 上传后原地转占位符(数据留盘上,刚写的别脱水);
/// 目录 → 远端 mkdir 后转占位符(开按需填充)
fn new_entry(local: &Path, remote: &str, ctx: &Ctx) -> anyhow::Result<Outcome> {
    use cloud_filter::placeholder::ConvertOptions;
    // 落盘时再验一次:可能是枚举/修复线程先转好的,那就没事了
    if let Ok(md) = std::fs::metadata(local) {
        if md.file_attributes() & 0x400 != 0 {
            return Ok(Outcome::Done);
        }
    } else {
        return Ok(Outcome::Done); // 没了:随后 Removed 事件管
    }
    let md = std::fs::metadata(local)?;
    if md.is_dir() {
        let id = match ctx.client.with(|c| c.mkdir(remote)) {
            Ok(id) => id,
            Err(e) if already_exists(&e) => resolve_remote_id(remote, ctx)
                .ok_or_else(|| anyhow::anyhow!("mkdir 报已存在但回查不到 {remote}"))?,
            Err(e) => return Err(e),
        };
        let blob = blob_for(id, true, 0, 0, remote);
        let f = std::fs::OpenOptions::new()
            .access_mode(0x4000_0000)
            .share_mode(7)
            .custom_flags(0x0200_0000) // 目录要 BACKUP_SEMANTICS
            .open(local)?;
        let mut ph = Placeholder::from(f);
        ph.convert_to_placeholder(
            ConvertOptions::default().blob(blob).mark_in_sync().has_children(),
            None,
        )?;
        ctx.dir_cache.remove(&core::parent_of(remote));
        tracing::info!("新目录已建远端并转占位符:{}", local.display());
        return Ok(Outcome::Done);
    }
    // 文件:上传 → 转占位符
    let new_id = core::upload_local_file(&ctx.client, &ctx.progress, local, remote, md.len())?;
    let id = match new_id {
        Some(id) => id,
        None => resolve_remote_id(remote, ctx).unwrap_or(0),
    };
    let blob = blob_for(id, false, md.len(), mtime_s(&md), remote);
    let f = std::fs::OpenOptions::new()
        .access_mode(0x4000_0000)
        .share_mode(7)
        .open(local)?;
    let mut ph = Placeholder::from(f);
    ph.convert_to_placeholder(ConvertOptions::default().blob(blob).mark_in_sync(), None)?;
    ctx.dir_cache.remove(&core::parent_of(remote));
    tracing::info!("新文件已上传并转占位符:{}({} 字节)", local.display(), md.len());
    Ok(Outcome::Done)
}

/// 普通文件本地改名 → 远端 mv。远端还没有源(排队上传期间就被改名)时,
/// 改按新名走 NewEntry 补传
fn remote_move(from_local: &Path, to_local: &Path, to_remote: &str, ctx: &Ctx) -> anyhow::Result<Outcome> {
    let Some(from_remote) = core::local_to_remote(from_local, &ctx.sync_root, &ctx.root) else {
        return Ok(Outcome::Done);
    };
    let dest = core::parent_of(to_remote);
    let newname = Path::new(to_remote)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match ctx.client.with(|c| c.mv(&from_remote, &dest, &newname)) {
        Ok(()) => {
            ctx.dir_cache.remove(&core::parent_of(&from_remote));
            ctx.dir_cache.remove(&dest);
            tracing::info!("远端已改名:{from_remote} → {to_remote}");
            Ok(Outcome::Done)
        }
        Err(e) if not_exist(&e) => {
            tracing::warn!("远端无 {from_remote}(改名前未传完),改为按新名补传");
            let _ = ctx.tx.send(Cmd::Added(to_local.to_path_buf()));
            Ok(Outcome::Done)
        }
        Err(e) => Err(e),
    }
}

/// 本地删除 → 远端删除。远端本就没有(没传过/已删)算成功
fn remote_delete(remote: &str, ctx: &Ctx) -> anyhow::Result<Outcome> {
    if let Err(e) = ctx.client.with(|c| c.delete(remote)) {
        if !not_exist(&e) {
            return Err(e);
        }
    }
    ctx.dir_cache.remove(&core::parent_of(remote));
    tracing::info!("远端已删:{remote}");
    Ok(Outcome::Done)
}

// ---------- 杂项 ----------

fn blob_for(id: u64, is_dir: bool, size: u64, mtime: i64, remote: &str) -> Vec<u8> {
    super::identity::encode(&NetFile {
        fs_id: id,
        path: remote.to_string(),
        name: Path::new(remote)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        is_dir,
        size,
        mtime,
    })
}

/// 按路径在父目录列表里回查 fs_id(秒传/异常时的兜底)
fn resolve_remote_id(remote: &str, ctx: &Ctx) -> Option<u64> {
    let parent = core::parent_of(remote);
    let files = ctx.client.with(|c| c.list_dir(&parent)).ok()?;
    files
        .iter()
        .find(|f| f.path.eq_ignore_ascii_case(remote))
        .map(|f| f.fs_id)
}

fn mtime_s(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn already_exists(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<crate::baidu::ApiError>(),
        Some(ae) if ae.errno == -8 || ae.errno == 31061
    )
}

fn not_exist(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<crate::baidu::ApiError>(),
        Some(ae) if ae.errno == -9 || ae.errno == 31066
    )
}

// ---------- watcher(ReadDirectoryChangesW,同步版) ----------

fn spawn_watcher(sync_root: PathBuf, tx: Sender<Cmd>) {
    std::thread::Builder::new()
        .name("syncback-watch".into())
        .spawn(move || watch_loop(sync_root, tx))
        .expect("起 syncback watcher 线程");
}

fn watch_loop(sync_root: PathBuf, tx: Sender<Cmd>) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        ReadDirectoryChangesW, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    };
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_LIST_DIRECTORY: u32 = 0x0001;

    let dir = loop {
        match std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(7)
            .custom_flags(BACKUP_SEMANTICS)
            .open(&sync_root)
        {
            Ok(f) => break f,
            Err(e) => {
                tracing::error!("watcher 打不开同步根:{e},5s 后重试");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_CREATION;
    // 64KB:大目录一次搬动上千条事件也不丢;真溢出(ERROR_NOTIFY_ENUM_DIR)
    // ReadDirectoryChangesW 返回失败,循环重来(丢的事件由下次脏扫兜底)
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut ret = 0u32;
        let ok = unsafe {
            ReadDirectoryChangesW(
                dir.as_raw_handle() as *mut _,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                1, // 递归整棵树
                filter,
                &mut ret,
                std::ptr::null_mut(),
                None,
            )
        };
        if ok == 0 || ret == 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!("watcher 读变更失败:{err}(缓冲溢出则丢事件,脏扫兜底)");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        emit_events(&buf[..ret as usize], &sync_root, &tx);
    }
}

/// 解析 FILE_NOTIFY_INFORMATION 链并投递。
/// 改名会连续来两条(OLD_NAME/NEW_NAME),攒起来配对发 Renamed
fn emit_events(bytes: &[u8], sync_root: &Path, tx: &Sender<Cmd>) {
    const ADDED: u32 = 1;
    const REMOVED: u32 = 2;
    const MODIFIED: u32 = 3;
    const RENAMED_OLD: u32 = 4;
    const RENAMED_NEW: u32 = 5;

    let mut off = 0usize;
    let mut pending_old: Option<PathBuf> = None;
    while off + 12 <= bytes.len() {
        let next = u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap());
        let name_len = u32::from_ne_bytes(bytes[off + 8..off + 12].try_into().unwrap()) as usize;
        if off + 12 + name_len > bytes.len() {
            break; // 半截记录:丢弃尾部
        }
        let name_bytes = &bytes[off + 12..off + 12 + name_len];
        let name: PathBuf = String::from_utf16_lossy(bytes_as_u16(name_bytes)).into();
        let full = sync_root.join(&name);
        match action {
            ADDED => {
                let _ = tx.send(Cmd::Added(full));
            }
            REMOVED => {
                let _ = tx.send(Cmd::Removed(full));
            }
            MODIFIED => {
                let _ = tx.send(Cmd::Touch(full));
            }
            RENAMED_OLD => {
                pending_old = Some(full);
            }
            RENAMED_NEW => {
                if let Some(old) = pending_old.take() {
                    let _ = tx.send(Cmd::Renamed(old, full));
                } else {
                    // 没配到 OLD(链断在上一块):按"新出现"兜底
                    let _ = tx.send(Cmd::Added(full));
                }
            }
            _ => {}
        }
        if next == 0 {
            break;
        }
        off += next;
    }
}

/// &[u8] → &[u16](长度必为偶数;不引 bytemuck 依赖)
fn bytes_as_u16(b: &[u8]) -> &[u16] {
    if b.len() % 2 != 0 {
        return &[];
    }
    // SAFETY:u16 与两个相邻 u8 同宽重组,来自 API 缓冲,生命周期同源
    unsafe { std::slice::from_raw_parts(b.as_ptr() as *const u16, b.len() / 2) }
}

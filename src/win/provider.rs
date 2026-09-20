//! WinProvider:cloud_filter 回调实现。
//! M2 起支持目录枚举(fetch_placeholders);M3 起支持水合/取消/脱水。
//! 回传(M4:closed/state_changed/dirty 扫)后续里程碑接入。

use crate::baidu::NetFile;
use crate::core;
use crate::progress::Progress;
use crate::settings::Settings;
use cloud_filter::error::{CloudErrorKind, CResult};
use cloud_filter::filter::{self, ticket, Request, SyncFilter};
use cloud_filter::metadata::{Metadata, MetadataExt};
use cloud_filter::placeholder::{PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_file::PlaceholderFile;
use std::collections::HashSet;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// pin 水合在途表(进程级;水合线程结束时跨线程清标记,thread_local 干不了)
static HYDRATING: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();

fn hydrating_set() -> &'static Mutex<HashSet<PathBuf>> {
    HYDRATING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 该文件是否正被水合(下载写盘中)。syncback 用它滤掉下载引发的写事件
pub(crate) fn is_hydrating(path: &Path) -> bool {
    hydrating_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

/// 脏文件集(本地改过未回传,进程级):syncback worker 标/清,
/// dehydrate 回调查——"释放空间"遇脏即拒,防丢改动。
/// 不在回调里现查 Placeholder:实测死锁(回调 open 同一文件等 oplock,
/// 而脱水正持有它,双方互等)
static DIRTY: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();

fn dirty_set() -> &'static Mutex<HashSet<PathBuf>> {
    DIRTY.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn mark_dirty(path: &Path) {
    dirty_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf());
}

pub(crate) fn clear_dirty(path: &Path) {
    dirty_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// 记一条"该文件正在 pin 水合中",防属性风暴重复触发;已在水合返回 false
fn mark_hydrating(path: &Path) -> bool {
    let mut h = hydrating_set().lock().unwrap_or_else(|e| e.into_inner());
    let fresh = !h.contains(path);
    if fresh {
        h.insert(path.to_path_buf());
    }
    fresh
}

/// 同步提供方:挂在 Explorer 里的一棵"百度网盘"树
pub struct WinProvider {
    /// 共享客户端(锁粒度见 core::SharedClient);Arc:syncback worker 持克隆
    pub(crate) client: std::sync::Arc<core::SharedClient>,
    /// 目录列表缓存(Windows 侧建议 dir_ttl=300:枚举风暴打不起配额)
    pub(crate) dir_cache: std::sync::Arc<core::DirCache>,
    /// dlink 缓存(水合用)
    pub(crate) dlink_cache: std::sync::Arc<core::DLinkCache>,
    /// 水合协调器(并发限 3 + 取消标志)
    pub(crate) hydrator: super::hydrate::Hydrator,
    /// 回传管线(M4):watcher + 防抖队列 + worker
    pub(crate) syncback: super::syncback::SyncBack,
    /// 远端根(网盘里的哪棵子树映射到同步根)
    pub(crate) root: String,
    /// 本地同步根绝对路径
    pub(crate) sync_root: PathBuf,
    /// 下载/上传进度(progress.json)
    pub(crate) progress: Progress,
}

impl WinProvider {
    pub fn new(st: &Settings, sync_root: PathBuf) -> anyhow::Result<Self> {
        let client = std::sync::Arc::new(core::SharedClient::new(crate::baidu::BaiduClient::from_config()?));
        let dir_cache = std::sync::Arc::new(core::DirCache::new(st.dir_ttl));
        let dlink_cache = std::sync::Arc::new(core::DLinkCache::new(st.dlink_ttl));
        let progress = Progress::new();
        let syncback = super::syncback::SyncBack::new(
            client.clone(),
            dir_cache.clone(),
            progress.clone(),
            sync_root.clone(),
            core::normalize_root(&st.root),
        );
        Ok(Self {
            client,
            dir_cache,
            dlink_cache,
            hydrator: super::hydrate::Hydrator::new(),
            syncback,
            root: core::normalize_root(&st.root),
            sync_root,
            progress,
        })
    }

    /// pin 状态对齐(OneDrive 式即时行为,平台本体两者都懒):
    /// - UNPINNED + 已回传 + 盘上有数据 → 立即脱水成云图标
    /// - PINNED + 盘上不完整 → 触发水合(平台会回调 fetch_data,进度条照常)
    /// 在 state_changed 的监视线程上跑,水合另开线程(它要下完才返回)
    fn sync_pin_state(&self, path: &Path) {
        let Ok(Some(pi)) = Placeholder::open(path)
            .and_then(|p| p.info().map_err(Into::into))
            .map_err(|e| {
                tracing::debug!("查 {} 占位符状态失败:{e}", path.display());
                e
            })
        else {
            return; // 非占位符(普通文件/新建文件):M4 回传管
        };
        let on_disk = pi.on_disk_data_size().max(0) as u64;
        match pi.pin_state() {
            PinState::Unpinned if pi.is_in_sync() && on_disk > 0 => {
                self.dehydrate_now(path);
            }
            PinState::Pinned if !pi.is_in_sync() => {
                // 脏文件被 pin:等 M4 回传完成后再说,现在别动
            }
            PinState::Pinned => {
                let logical = std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX);
                if on_disk >= logical {
                    // 已完整落地:水合若在途,清掉标记
                    hydrating_set()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(path);
                } else if mark_hydrating(path) {
                    let p = path.to_path_buf();
                    std::thread::spawn(move || {
                        let r = Placeholder::open(&p).and_then(|mut ph| ph.hydrate(..));
                        match r {
                            Ok(()) => tracing::info!("pin 水合完成:{}", p.display()),
                            Err(e) => tracing::warn!("pin 水合 {} 失败:{e}", p.display()),
                        }
                        hydrating_set()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&p);
                    });
                }
            }
            _ => {}
        }
    }

    /// 立即脱水(独占写句柄由平台要求;Explorer 松手要一两秒,重试几下)
    fn dehydrate_now(&self, path: &Path) {
        for i in 0..3 {
            let Ok(f) = std::fs::OpenOptions::new()
                .access_mode(0x4000_0000) // GENERIC_WRITE
                .share_mode(0) // 独占
                .open(path)
            else {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            let mut ph = Placeholder::from(f);
            match ph.update(UpdateOptions::default().dehydrate(), None) {
                Ok(_) => {
                    tracing::info!("已脱水(用户释放空间):{}", path.display());
                    return;
                }
                Err(e) => {
                    tracing::warn!("脱水 {} 失败(第{}次):{e}", path.display(), i + 1);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    }

    /// 解析水合目标的 fs_id:优先身份 blob;id=0(秒传)或 blob 坏时
    /// 按父目录列表回查(本地路径大小写可能与远端不一致,忽略大小写匹配)。
    /// 查不到返回 0,由水合管线发失败应答
    fn resolve_fs_id(&self, request: &Request, remote: &str) -> u64 {
        if let Some(id) = super::identity::decode(request.file_blob()).filter(|i| i.id != 0) {
            return id.id;
        }
        let parent = core::parent_of(remote);
        match self.dir_cache.get_or_fetch(&self.client, &parent) {
            Ok(files) => {
                let hit = files
                    .iter()
                    .find(|f| f.path.eq_ignore_ascii_case(remote))
                    .map(|f| f.fs_id);
                match hit {
                    Some(id) => {
                        tracing::debug!("fs_id 回查命中:{remote} -> {id}");
                        id
                    }
                    None => {
                        tracing::warn!("父目录里找不到 {remote},无法解析 fs_id");
                        0
                    }
                }
            }
            Err(e) => {
                tracing::warn!("回查 fs_id 列 {parent} 失败:{e:#}");
                0
            }
        }
    }
}

impl SyncFilter for WinProvider {
    /// 水合:按 required 区间分块下载进占位符(管线在 hydrate.rs)。
    /// 错误一律在回调内用 ticket 带区间上报后返回 Ok——直接返回 Err 会走
    /// proxy 的同步失败应答(全 0 区间,被平台拒绝,用户等 60s 超时),
    /// 见 vendor/cloud-filter/PATCHES.md
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: filter::info::FetchData,
    ) -> CResult<()> {
        let local = request.path();
        let Some(remote) = core::local_to_remote(&local, &self.sync_root, &self.root) else {
            tracing::error!("水合请求在同步根之外:{},拒绝", local.display());
            if let Err(e) = ticket.fail(CloudErrorKind::NotUnderSyncRoot, info.required_file_range())
            {
                tracing::warn!("失败应答未送达(平台将按超时处理):{e}");
            }
            return Ok(());
        };
        if info.interrupted_hydration() {
            tracing::info!("续传(上次水合被中断):{remote}");
        }
        let fs_id = self.resolve_fs_id(&request, &remote);
        self.hydrator.fetch(
            self,
            &remote,
            fs_id,
            request.file_size(),
            info.required_file_range(),
            ticket,
        )
    }

    /// 取消水合:置取消标志,块间生效(已写部分保留,天然断点续传)
    fn cancel_fetch_data(&self, request: Request, info: filter::info::CancelFetchData) {
        self.hydrator.cancel(&request.path(), &info);
    }

    /// 脱水("释放空间"):干净(已回传)才放行;脏文件拒绝,防丢本地改动。
    /// 脏判定用进程内 DIRTY 集(syncback 入队时标、传完清),不现查
    /// Placeholder(死锁,见 DIRTY 注释)
    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        info: filter::info::Dehydrate,
    ) -> CResult<()> {
        let path = request.path();
        let dirty = dirty_set()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&path);
        if dirty {
            tracing::warn!("拒绝脱水(本地改动未回传):{}", path.display());
            // Dehydrate 的失败应答没有区间字段,Err 路径可用(proxy 已不 unwrap)
            return Err(CloudErrorKind::Unsuccessful);
        }
        tracing::info!(
            "放行脱水:{}(background={},reason={:?})",
            path.display(),
            info.background(),
            info.reason()
        );
        let r = ticket.pass();
        tracing::info!("脱水应答已发:{} -> {r:?}", path.display());
        r.map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// 脱水完成通知:仅记日志(dlink 缓存按 TTL 自然过期即可)
    fn dehydrated(&self, request: Request, info: filter::info::Dehydrated) {
        tracing::info!(
            "已脱水:{}(background={})",
            request.path().display(),
            info.background()
        );
    }

    /// 属性变更回调(crate 用 ReadDirectoryChangesW 监听,pin/unpin 也算属性):
    /// 把文件对齐到用户要的 pin 状态(unpin→脱水回云图标;pin→触发水合)
    fn state_changed(&self, changes: Vec<PathBuf>) {
        for p in changes {
            self.sync_pin_state(&p);
        }
    }

    /// 目录枚举:Explorer 展开目录时回调,把远端条目批量建成本地占位符。
    /// PopulationType::Full 的语义就是"按目录惰性填充",不展开的目录不打 API。
    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: filter::info::FetchPlaceholders,
    ) -> CResult<()> {
        let dir = request.path();
        let Some(remote) = core::local_to_remote(&dir, &self.sync_root, &self.root) else {
            tracing::warn!("枚举请求在同步根之外:{},拒绝", dir.display());
            return Err(CloudErrorKind::NotUnderSyncRoot);
        };
        let files = match self.dir_cache.get_or_fetch(&self.client, &remote) {
            Ok(f) => f,
            Err(e) => {
                // 配额/网络挂了:宁可端出旧缓存也别让已看过的目录整个消失
                if let Some(stale) = self.dir_cache.stale(&remote) {
                    tracing::warn!("列 {remote} 失败({e:#}),用旧缓存 {} 条", stale.len());
                    stale
                } else {
                    tracing::error!("列 {remote} 失败且无旧缓存:{e:#}");
                    return Err(map_cloud_err(&e));
                }
            }
        };

        // 本地已有同名条目(占位符或真实文件)跳过:重展开只补缺,不覆盖。
        // 例外:同名"普通目录"(没有 reparse,多为进程暴死时枚举回调被打断
        // 留下的残迹)要转回占位符,否则它永远不会再触发按需填充
        let existing = list_local_entries(&dir);
        let mut phs: Vec<PlaceholderFile> = Vec::with_capacity(files.len());
        for f in &files {
            if !win_name_ok(&f.name) {
                tracing::warn!("跳过 Windows 非法名:{}", f.path);
                continue;
            }
            if let Some(ent) = existing.get(&f.name.to_lowercase()) {
                if f.is_dir && ent == &LocalEntry::PlainDir {
                    let local = dir.join(&f.name);
                    std::thread::spawn(move || repair_plain_dir(&local));
                }
                continue;
            }
            push_placeholder(&mut phs, f);
        }
        ticket
            .pass_with_placeholder(&mut phs)
            .map_err(|_| CloudErrorKind::Unsuccessful)?;
        tracing::debug!("填充 {remote}:建 {} 条(共 {} 条,其余为已有/非法名)", phs.len(), files.len());
        Ok(())
    }

    /// 写句柄(打开时有写/删权限)关闭:文件可能被改过 → 交给 syncback
    /// 分类(占位符且 in_sync 被内核清了才会上传)
    fn closed(&self, request: Request, info: filter::info::Closed) {
        if info.deleted() {
            return; // 关句柄顺带删了文件:deleted 回调管
        }
        self.syncback.touch(request.path());
    }

    /// 占位符将被删除:先删远端(秒级,回调内直接做),成了才放行;
    /// 失败返回 Err —— Explorer 会把删除弹回来,忠实反馈"网盘删不掉"
    fn delete(
        &self,
        request: Request,
        ticket: ticket::Delete,
        info: filter::info::Delete,
    ) -> CResult<()> {
        let path = request.path();
        if info.is_undelete() {
            // 回收站还原之类:本地操作照放,远端由 NewEntry/枚举对齐
            return ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful);
        }
        let Some(remote) = core::local_to_remote(&path, &self.sync_root, &self.root) else {
            return ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful);
        };
        tracing::info!("删除 {remote}(is_dir={})", info.is_directory());
        match self.client.with(|c| c.delete(&remote)) {
            Ok(()) => {
                self.dir_cache.remove(&core::parent_of(&remote));
                self.syncback.mark_handled(path); // 抵掉 watcher 的 Removed
                ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
            }
            Err(e) if crate::baidu::is_forbidden(&e) => {
                tracing::error!("远端删除 {remote} 被拒:{e:#}");
                Err(CloudErrorKind::AccessDenied)
            }
            Err(e) => {
                tracing::error!("远端删除 {remote} 失败:{e:#}");
                Err(map_cloud_err(&e))
            }
        }
    }

    /// 占位符将被改名/移动(占位符才有此回调;普通文件走 syncback watcher):
    /// - 根内改名/移动 → 远端 mv,失败弹回
    /// - 移出同步根(进回收站等)→ 远端删源
    /// - 从根外移入 → 放行,目标按新条目补传
    fn rename(
        &self,
        request: Request,
        ticket: ticket::Rename,
        info: filter::info::Rename,
    ) -> CResult<()> {
        let from = request.path();
        let to = info.target_path();
        if !info.source_in_scope() {
            if info.target_in_scope() {
                // 外部拖入:本地已是现成文件,转占位符/上传交给 syncback
                self.syncback.new_entry(to);
            }
            return ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful);
        }
        let Some(from_remote) = core::local_to_remote(&from, &self.sync_root, &self.root) else {
            return ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful);
        };
        if !info.target_in_scope() {
            // 移出根 = 从这棵树消失:远端删源,失败弹回(别让本地先没)
            tracing::info!("移出同步根,远端删源:{from_remote}");
            return match self.client.with(|c| c.delete(&from_remote)) {
                Ok(()) => {
                    self.dir_cache.remove(&core::parent_of(&from_remote));
                    self.syncback.mark_handled(from);
                    ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
                }
                Err(e) => {
                    tracing::error!("移出时远端删 {from_remote} 失败:{e:#}");
                    Err(map_cloud_err(&e))
                }
            };
        }
        let Some(to_remote) = core::local_to_remote(&to, &self.sync_root, &self.root) else {
            return ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful);
        };
        let dest = core::parent_of(&to_remote);
        let newname = to
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        tracing::info!("改名 {from_remote} → {to_remote}(is_dir={})", info.is_directory());
        match self.client.with(|c| c.mv(&from_remote, &dest, &newname)) {
            Ok(()) => {
                self.dir_cache.remove(&core::parent_of(&from_remote));
                self.dir_cache.remove(&dest);
                // 抵掉 watcher 的 Renamed,以及平台改名后清 in_sync
                // 必然补发的 Touch(否则改名会被误判成改动而整文件重传)
                self.syncback.mark_handled(from);
                self.syncback.mark_handled(to);
                ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
            }
            Err(e) => {
                tracing::error!("远端改名 {from_remote} 失败:{e:#}");
                Err(map_cloud_err(&e))
            }
        }
    }

    /// 删除完成通知:记日志(远端在 delete 回调里已处理)
    fn deleted(&self, request: Request, _info: filter::info::Deleted) {
        tracing::info!("已删除:{}", request.path().display());
    }

    /// 改名完成通知:记日志(远端在 rename 回调里已处理)
    fn renamed(&self, request: Request, info: filter::info::Renamed) {
        tracing::info!(
            "已改名:{} → {}",
            info.source_path().display(),
            request.path().display()
        );
    }
}

/// 启动扫:把可能撕裂的占位符目录(丢了 reparse 的普通目录)转回占位符。
/// fetch_placeholders 里的同款修复只在新枚举时生效,而"已填充"的目录平台
/// 不再回调,所以启动时得自己走一遍。纯本地 read_dir(不打 API),秒级。
/// M4 上线后新建本地目录归回传管线管,届时要避开刚建的(按 mtime)
pub(crate) fn repair_torn_dirs(root: &Path) {
    walk_repair(root, 0);
}

fn walk_repair(dir: &Path, depth: usize) {
    // 同步根本身不是占位符,从子级开始扫;深度封顶防御环/超深树
    if depth > 8 {
        return;
    }
    use std::os::windows::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        if !e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let plain = e
            .metadata()
            .map(|md| md.file_attributes() & 0x400 /* REPARSE_POINT */ == 0)
            .unwrap_or(true);
        let p = e.path();
        if plain {
            repair_plain_dir(&p);
        } else {
            // 已是占位符但本地空着:可能上次转换没带 on-demand 标志
            // (平台视为"已填充完毕",永远不再回调)。update 补开,幂等
            if std::fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(false) {
                reenable_population(&p);
            }
        }
        walk_repair(&e.path(), depth + 1);
    }
}

/// 给"已填充完毕"态的空占位符目录重开按需填充(update 版,句柄要求
/// 同 convert:GENERIC_WRITE + BACKUP_SEMANTICS;目录常被 Explorer 持着,
/// 独占会失败,全共享 + 重试)
fn reenable_population(local: &Path) {
    use cloud_filter::placeholder::UpdateOptions;
    for i in 0..3 {
        let Ok(f) = std::fs::OpenOptions::new()
            .access_mode(0x4000_0000) // GENERIC_WRITE
            .share_mode(7)
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(local)
        else {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        };
        let mut ph = Placeholder::from(f);
        return match ph.update(UpdateOptions::default().has_children(), None) {
            Ok(_) => tracing::debug!("已重开按需填充:{}", local.display()),
            Err(e) => {
                tracing::warn!("重开按需填充 {} 失败(第{}次):{e}", local.display(), i + 1);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        };
    }
}

/// 本地目录条目分类(枚举跳过 + 残迹修复用)
#[derive(PartialEq, Eq)]
enum LocalEntry {
    PlainDir,
    Other,
}

/// 列出目录下已有名字 → 分类;读不了当空表
fn list_local_entries(dir: &std::path::Path) -> std::collections::HashMap<String, LocalEntry> {
    use std::collections::HashMap;
    use std::os::windows::fs::MetadataExt;
    let mut m = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return m;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_lowercase();
        let plain_dir = e
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or(false)
            && !e
                .metadata()
                .map(|md| {
                    md.file_attributes() & 0x400 /* FILE_ATTRIBUTE_REPARSE_POINT */ != 0
                })
                .unwrap_or(true);
        m.insert(name, if plain_dir { LocalEntry::PlainDir } else { LocalEntry::Other });
    }
    m
}

/// 把普通目录转回占位符目录(blob 补不回来——身份用 id=0 占位,
/// 水合/回传时会按父目录回查)。has_children 置 on-demand population:
/// 转完平台在下次展开时回调 fetch_placeholders 补条目
/// (不带的话默认视为"已填充完毕",目录会一直空着)
fn repair_plain_dir(local: &Path) {
    use cloud_filter::placeholder::ConvertOptions;
    use std::os::windows::fs::OpenOptionsExt;
    let blob = super::identity::encode(&NetFile {
        fs_id: 0,
        path: String::new(),
        name: local.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        is_dir: true,
        size: 0,
        mtime: 0,
    });
    let f = match std::fs::OpenOptions::new()
        .access_mode(0x4000_0000) // GENERIC_WRITE(convert 要求)
        .share_mode(7)
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS(目录必须)
        .open(local)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("修复目录 {} 打不开:{e}", local.display());
            return;
        }
    };
    let mut ph = Placeholder::from(f);
    match ph.convert_to_placeholder(
        ConvertOptions::default().blob(blob).mark_in_sync().has_children(),
        None,
    ) {
        Ok(_) => tracing::info!("已把普通目录转回占位符:{}", local.display()),
        Err(e) => tracing::warn!("转换 {} 失败:{e}", local.display()),
    }
}

/// 建一个占位符条目(文件带大小/目录带目录属性,时间都取远端 mtime)
fn push_placeholder(out: &mut Vec<PlaceholderFile>, f: &NetFile) {
    let ft = filetime(f.mtime);
    let md = if f.is_dir {
        Metadata::directory()
    } else {
        Metadata::file().size(f.size)
    }
    .last_write_time(ft)
    .creation_time(ft)
    .last_access_time(ft);
    out.push(
        PlaceholderFile::new(&f.name)
            .metadata(md)
            .blob(super::identity::encode(f))
            .mark_in_sync(),
    );
}

/// epoch 秒 → Windows FILETIME(1601-01-01 起、100ns 单位)
fn filetime(unix_secs: i64) -> i64 {
    (unix_secs + 11_644_473_600) * 10_000_000
}

/// Windows 合法文件名:禁字符/尾点尾空格/保留设备名都过不了 NTFS,
/// 远端有这种名字就跳过 + warn(不去改名,保持"看到的就是网盘里的")
fn win_name_ok(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
    {
        return false;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    // 保留设备名(不分大小写,CON.txt 也算):取首个点前的一段
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return false;
    }
    if let Some(num) = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")) {
        if num.len() == 1 && num.bytes().all(|b| (b'1'..=b'9').contains(&b)) {
            return false;
        }
    }
    true
}

/// 把 anyhow 错误翻成给平台的反馈:网络类 → NetworkUnavailable,其余 → Unsuccessful
fn map_cloud_err(e: &anyhow::Error) -> CloudErrorKind {
    tracing::warn!("回调失败:{e:#}");
    if crate::baidu::is_forbidden(e) {
        CloudErrorKind::AccessDenied
    } else {
        CloudErrorKind::Unsuccessful
    }
}

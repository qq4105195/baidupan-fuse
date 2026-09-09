//! FUSE 文件系统:把百度网盘目录树映射成本地挂载点(读 + 写)。
//!
//! 设计要点:
//! - fuser 是 inode 协议、百度 API 是路径协议,这里维护 ino↔path 双向表;
//!   inode 单调递增不复用,会话内稳定。
//! - 目录列表/attr 走内存缓存(TTL 可配),否则内核一次 ls 触发的几十个
//!   lookup/getattr 会把 API 配额(未过审 10 次/小时)瞬间打爆。
//! - mount2 默认单线程串行分发请求,天然限制了并发打 API,对配额友好
//!   (代价是大目录 readdir 会阻塞其他操作,v1 接受)。
//! - 写走"本地暂存 + close 时上传":改动全落 config 目录 uploads/ 下的
//!   暂存文件,flush/release 时按官方三段式(precreate → superfile2 分片
//!   → create)传回网盘,分片进度复用下载那套 progress.json。

use crate::baidu::{upload_slice_size, ApiError, BaiduClient, NetFile};
use anyhow::anyhow;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// fuser 约定的根 inode
const ROOT_INO: u64 = 1;
/// 回给内核的 attr/entry 有效期(配合内核缓存,没变就不反复来问)
const TTL: Duration = Duration::from_secs(60);
/// 判定"顺序读"的容差:上一次读的结尾和这次的起点差在这个窗口内,就预读下一块
const SEQ_TOLERANCE: u64 = 256 * 1024;

/// 路径拼接:父目录是 "/" 时不要出现 "//"
pub fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// 父目录:"/" 的父还是 "/"
pub fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => "/".to_string(),
    }
}

/// 一次写会话:改动先落本地暂存文件,close(flush)时三段式传回网盘。
/// [0,len) 已物化;[len,target) 是"旧远端数据/0"的虚段,上传前统一回填
/// (暂存文件是新建的,没写过的区间天然读作 0,只有旧远端数据要真回填)
struct WriteSession {
    /// 远端目标路径(rename 时同步改)
    path: String,
    /// 暂存文件句柄常开,seek+write 直接写
    file: std::fs::File,
    /// 暂存文件路径(config 目录 uploads/ 下)
    tmp: PathBuf,
    /// 本地已物化前缀长度
    len: u64,
    /// 逻辑大小(getattr 看到的;可大于 len,中间是虚段)
    target: u64,
    /// 远端旧文件的 fs_id;0 = 远端还没有这个文件
    fs_id: u64,
    /// 远端旧文件大小(回填的边界)
    remote_size: u64,
    /// 有未上传的改动
    dirty: bool,
}

pub struct PanFs {
    client: BaiduClient,
    /// 挂载的远端根目录(未过审应用只能访问 /apps/<应用名>,用它挂对应目录)
    /// 目前只做记录,路径解析走 ino↔path 表;后续做挂载内路径校验/日志用
    #[allow(dead_code)]
    root: String,
    /// 目录列表缓存时长
    dir_ttl: Duration,
    /// 下载直链缓存时长(官方 8 小时有效,默认保守取 30 分钟)。
    /// 实测顺序复用同一 dlink 没问题,之前的 403 是并发波浪触发的
    dlink_ttl: Duration,
    /// fs_id -> (dlink, 拉取时刻)
    dlink_cache: HashMap<u64, (String, Instant)>,
    next_ino: u64,
    ino_of: HashMap<String, u64>,
    path_of: HashMap<u64, String>,
    /// 目录路径 -> (条目列表, 拉取时刻)
    dir_cache: HashMap<String, (Vec<NetFile>, Instant)>,
    // ---- 顺序读加速:块缓存 + 预读 ----
    /// 块大小(字节)。内核单次 read 上限 128KB,直接打 API 每次一个
    /// HTTP 往返(实测 160KB/s);按块拉取+缓存后顺序读 ≈ 单块拉取速度。
    /// 实测块越大吞吐越高(连接爬坡摊薄):8MB≈4MB/s,16MB≈7MB/s,32MB 持平
    block_size: u64,
    /// 每块并发连接数。实测 SVIP 账号"单长流"才是高速通道,
    /// 并发波浪会被 CDN 限速(8 并发分块反而只有 ~2.3MB/s),默认 1;
    /// 保留参数是给非 SVIP 账号实验用
    parallel: usize,
    /// (ino, 块号) -> 数据
    blocks: HashMap<(u64, u64), Vec<u8>>,
    /// 块的入场顺序,FIFO 淘汰用
    block_order: VecDeque<(u64, u64)>,
    /// 当前缓存字节数
    cached_bytes: usize,
    /// 缓存字节上限
    cache_cap: usize,
    /// 上一次 read 的 (ino, 结束偏移),用来识别顺序读触发预读
    last_read: Option<(u64, u64)>,
    /// 连续顺序读的次数,用来做预读梯度:刚 seek 完不预读,读够 ~2MB 才放量
    /// (否则 seek 后一个 1MB 的小读会触发 3×16MB 的预读,延迟爆炸)
    seq_streak: u32,
    /// 下载进度记录(写 progress.json,控制台「8. 传输进度」看)
    progress: crate::progress::Progress,
    // ---- 写支持 ----
    /// ino -> 写会话(文件关掉最后一个写句柄时移除)
    writes: HashMap<u64, WriteSession>,
    /// ino -> 当前打开的写句柄数(fuser 没有 per-fh 状态,fh 里编码了写标志)
    write_refs: HashMap<u64, u32>,
}

impl PanFs {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: BaiduClient,
        root: &str,
        dir_ttl: u64,
        dlink_ttl: u64,
        block_mb: u64,
        parallel: u64,
        cache_mb: u64,
    ) -> Self {
        // 规范化:必须以 / 开头,去掉末尾 /(根目录保留 "/")
        let mut root = root.trim_end_matches('/').to_string();
        if !root.starts_with('/') {
            root = format!("/{root}");
        }
        if root.is_empty() {
            root = "/".to_string();
        }
        let mut fs = Self {
            client,
            root: root.clone(),
            dir_ttl: Duration::from_secs(dir_ttl.max(1)),
            dlink_ttl: Duration::from_secs(dlink_ttl.max(1)),
            dlink_cache: HashMap::new(),
            next_ino: ROOT_INO + 1,
            ino_of: HashMap::new(),
            path_of: HashMap::new(),
            dir_cache: HashMap::new(),
            block_size: (block_mb.max(1) << 20),
            parallel: parallel.max(1) as usize,
            blocks: HashMap::new(),
            block_order: VecDeque::new(),
            cached_bytes: 0,
            cache_cap: (cache_mb.max(16) << 20) as usize,
            last_read: None,
            seq_streak: 0,
            progress: crate::progress::Progress::new(),
            writes: HashMap::new(),
            write_refs: HashMap::new(),
        };
        // 写支持的暂存目录:挂载时清一遍,不留上次崩溃的残渣
        let updir = crate::baidu::config_dir().join("uploads");
        let _ = std::fs::remove_dir_all(&updir);
        if let Err(e) = std::fs::create_dir_all(&updir) {
            tracing::warn!("建暂存目录 {} 失败:{e}(写功能不可用)", updir.display());
        }
        fs.ino_of.insert(root.clone(), ROOT_INO);
        fs.path_of.insert(ROOT_INO, root);
        fs
    }

    /// 给路径分配 inode(已有则复用)
    fn alloc_ino(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.ino_of.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_of.insert(path.to_string(), ino);
        self.path_of.insert(ino, path.to_string());
        ino
    }

    /// 列目录(带 TTL 缓存)。返回克隆,避免和后续 &mut 调用打架。
    fn list(&mut self, dir: &str) -> anyhow::Result<Vec<NetFile>> {
        if let Some((files, at)) = self.dir_cache.get(dir) {
            if at.elapsed() < self.dir_ttl {
                return Ok(files.clone());
            }
        }
        let files = self.client.list_dir(dir)?;
        tracing::debug!("拉取目录 {dir}: {} 条", files.len());
        self.dir_cache.insert(dir.to_string(), (files.clone(), Instant::now()));
        Ok(files)
    }

    /// 拿 dlink(带 TTL 缓存)。实测顺序复用同一 dlink 完全没问题,
    /// filemetas 便宜但也没必要每块一查;缓存过期/被 403 时自然换新
    fn dlink(&mut self, fs_id: u64) -> anyhow::Result<String> {
        if let Some((d, at)) = self.dlink_cache.get(&fs_id) {
            if at.elapsed() < self.dlink_ttl {
                return Ok(d.clone());
            }
        }
        let d = self.client.get_dlink(fs_id)?;
        self.dlink_cache.insert(fs_id, (d.clone(), Instant::now()));
        Ok(d)
    }

    /// 从父目录列表里找指定路径的条目( getattr/read 复用 )
    fn find(&mut self, path: &str) -> anyhow::Result<NetFile> {
        let parent = parent_of(path);
        let files = self.list(&parent)?;
        files
            .into_iter()
            .find(|f| f.path == path)
            .ok_or_else(|| anyhow!("{} 不在父目录 {} 里(可能已被删除)", path, parent))
    }

    /// 暴露进度记录器给挂载入口(卸载后清进度文件用)
    pub fn progress(&self) -> &crate::progress::Progress {
        &self.progress
    }

    /// 统一的带重试拉取:403(dlink 失效/被限)时弃缓存换新链再来,
    /// 最多 2 次,带递增退避。整块拉取和随机读窗口共用。
    /// path/kind 只用于进度展示
    fn fetch_with_retry(
        &mut self,
        fs_id: u64,
        path: &str,
        kind: &str,
        off: u64,
        len: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            let dlink = self.dlink(fs_id)?;
            let id = self.progress.begin(path, kind, off, len);
            let r = self
                .client
                .read_range(&dlink, off, len, self.parallel, &self.progress, id);
            self.progress.end(id, r.is_ok());
            match r {
                Ok(d) => return Ok(d),
                Err(e) if attempt < 2 && crate::baidu::is_forbidden(&e) => {
                    attempt += 1;
                    // 403 多半意味着这条 dlink 已被限/失效,弃缓存下次换新链
                    self.dlink_cache.remove(&fs_id);
                    tracing::warn!("下载 403,弃 dlink 缓存换新链重试(第 {attempt} 次)");
                    std::thread::sleep(Duration::from_millis(300 * attempt as u64));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 确保某块在缓存里:没有就整块拉(内部并发数由 --parallel 决定),
    /// 超容量按 FIFO 淘汰旧块。
    fn ensure_block(
        &mut self,
        ino: u64,
        path: &str,
        fs_id: u64,
        fsize: u64,
        idx: u64,
    ) -> anyhow::Result<()> {
        if self.blocks.contains_key(&(ino, idx)) {
            return Ok(());
        }
        let off = idx * self.block_size;
        let len = self.block_size.min(fsize - off);
        let t0 = Instant::now();

        let data = self.fetch_with_retry(fs_id, path, &format!("块#{idx}"), off, len)?;

        tracing::debug!(
            "拉块 ino={ino} #{idx}: {} 字节,{} 并发,耗时 {:.2}s",
            data.len(),
            self.parallel,
            t0.elapsed().as_secs_f64()
        );
        // 防御:拉回来的数据必须和请求等长,短了会导致内核把短读当 EOF(文件看起来被截断)
        if data.len() as u64 != len {
            anyhow::bail!(
                "块 #{idx} 拉取不完整:要 {len} 字节,得到 {} 字节",
                data.len()
            );
        }
        self.cached_bytes += data.len();
        self.block_order.push_back((ino, idx));
        self.blocks.insert((ino, idx), data);
        while self.cached_bytes > self.cache_cap {
            match self.block_order.pop_front() {
                Some(k) => {
                    if let Some(v) = self.blocks.remove(&k) {
                        self.cached_bytes -= v.len();
                    }
                }
                None => break,
            }
        }
        Ok(())
    }

    fn attr(&self, req: &Request<'_>, ino: u64, nf: Option<&NetFile>) -> FileAttr {
        let (kind, size, mtime) = match nf {
            None => (FileType::Directory, 4096, SystemTime::now()),
            Some(f) => (
                if f.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                f.size,
                UNIX_EPOCH + Duration::from_secs(f.mtime.max(0) as u64),
            ),
        };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: if kind == FileType::Directory {
                0o755
            } else {
                0o644
            },
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: req.uid(),
            gid: req.gid(),
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    // ---------- 写支持 ----------

    /// 建写会话:暂存文件按 pid+ino 命名(一次挂载内唯一),
    /// truncate 新建,旧内容通过回填按需从远端拉
    fn open_write(
        &mut self,
        ino: u64,
        path: String,
        fs_id: u64,
        remote_size: u64,
    ) -> anyhow::Result<()> {
        if self.writes.contains_key(&ino) {
            return Ok(());
        }
        let dir = crate::baidu::config_dir().join("uploads");
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{}-{ino}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        tracing::debug!("开写会话 ino={ino} -> {}", tmp.display());
        self.writes.insert(
            ino,
            WriteSession {
                path,
                file,
                tmp,
                len: 0,
                target: remote_size,
                fs_id,
                remote_size,
                dirty: false,
            },
        );
        Ok(())
    }

    /// 查远端现状:返回 (fs_id, size);不存在 → (0,0)。
    /// 网络错误原样往外抛(映射成 EIO),绝不把"查询失败"当"新文件"
    fn stat_remote(&mut self, path: &str) -> anyhow::Result<(u64, u64)> {
        let parent = parent_of(path);
        let files = self.list(&parent)?;
        match files.into_iter().find(|f| f.path == path) {
            Some(f) if f.is_dir => Err(anyhow::Error::new(ApiError {
                errno: -7,
                msg: format!("{path} 是目录"),
            })),
            Some(f) => Ok((f.fs_id, f.size)),
            None => Ok((0, 0)),
        }
    }

    /// 惰性建写会话(open 时没建,write/setattr 半路杀进来时用)
    fn ensure_session_for(&mut self, ino: u64) -> anyhow::Result<()> {
        let Some(path) = self.path_of.get(&ino).cloned() else {
            anyhow::bail!("inode {ino} 没有路径映射");
        };
        let (fs_id, remote_size) = self.stat_remote(&path)?;
        self.open_write(ino, path, fs_id, remote_size)
    }

    /// 把 [len,to) 的虚段物化:旧远端数据逐块回填进暂存文件。
    /// 超过 remote_size 的部分不用写——暂存文件没写过的区间本来就读作 0
    fn backfill(&mut self, ino: u64, to: u64) -> anyhow::Result<()> {
        let (path, fs_id, remote_size, cur) = match self.writes.get(&ino) {
            Some(s) => (s.path.clone(), s.fs_id, s.remote_size, s.len),
            None => return Ok(()),
        };
        if to <= cur {
            return Ok(());
        }
        let remote_end = to.min(remote_size);
        let step = 1u64 << 20;
        let mut pos = cur;
        while pos < remote_end {
            let n = step.min(remote_end - pos);
            let data = self.fetch_with_retry(fs_id, &path, "回填", pos, n)?;
            if data.len() as u64 != n {
                anyhow::bail!("回填数据不完整 @{pos}:要 {n} 字节,得到 {}", data.len());
            }
            let Some(s) = self.writes.get_mut(&ino) else {
                return Ok(());
            };
            s.file.seek(SeekFrom::Start(pos))?;
            s.file.write_all(&data)?;
            pos += n;
            s.len = pos;
        }
        // [remote_end,to) 是 0 的虚段:把暂存文件稀疏扩到 to(没写过的区间读作 0)。
        // 不扩的话,上传时 read_exact_at 越过物理文件尾会报 failed to fill whole buffer
        if let Some(s) = self.writes.get_mut(&ino) {
            if to > s.len {
                s.file.set_len(to)?;
                s.len = to;
            }
        }
        Ok(())
    }

    /// 上传会话:补虚段 → 逐片算 md5 → precreate → 传缺片 → create。
    /// 秒传(precreate 返回 None)直接收尾
    fn upload_session(&mut self, ino: u64) -> anyhow::Result<()> {
        let (path, tmp, target) = match self.writes.get(&ino) {
            Some(s) => (s.path.clone(), s.tmp.clone(), s.target),
            None => return Ok(()),
        };
        // 1) 补齐 [len,target) 虚段
        self.backfill(ino, target)?;

        // 2) 分片规则:4MB 起步,>4GB 自动放大,保证 ≤1024 片(官方上限)
        let slice = upload_slice_size(target);
        let nblocks = target.div_ceil(slice);
        let f = std::fs::File::open(&tmp)?;
        let mut buf = vec![0u8; slice as usize];
        let mut md5s: Vec<String> = Vec::with_capacity(nblocks as usize);
        if nblocks == 0 {
            // 空文件:block_list=[] 会被 precreate 拒(errno=2)。
            // 用空串 md5 当唯一分片,实测 precreate 直接回 need=[] 免传,create 收尾
            md5s.push(format!("{:x}", md5::compute(&[])));
        }
        for i in 0..nblocks {
            let want = slice.min(target - i * slice) as usize;
            f.read_exact_at(&mut buf[..want], i * slice)?;
            md5s.push(format!("{:x}", md5::compute(&buf[..want])));
        }

        // 3~5) 三段式
        let new_id = match self.client.precreate(&path, target, &md5s)? {
            None => None, // 秒传
            Some((uploadid, need)) => {
                for seq in need {
                    if seq >= nblocks {
                        anyhow::bail!("precreate 要分片#{seq},本地只有 {nblocks} 片");
                    }
                    let off = seq * slice;
                    let want = slice.min(target - off) as usize;
                    let mut data = vec![0u8; want];
                    f.read_exact_at(&mut data, off)?;
                    self.client
                        .upload_slice(&uploadid, &path, seq, &data, &self.progress)?;
                }
                Some(self.client.create_file(&path, target, &uploadid, &md5s)?)
            }
        };
        drop(f);
        self.finish_upload(ino, new_id);
        tracing::info!("上传完成:{path}({target} 字节,{} 片)", nblocks);
        Ok(())
    }

    /// 上传成功后的收尾:改会话元数据、失效父目录列表/dlink/读块缓存
    fn finish_upload(&mut self, ino: u64, new_id: Option<u64>) {
        let old_id = self.writes.get(&ino).map(|s| s.fs_id).unwrap_or(0);
        let parent = self
            .writes
            .get(&ino)
            .map(|s| parent_of(&s.path))
            .unwrap_or_default();
        if let Some(s) = self.writes.get_mut(&ino) {
            // 秒传拿不到新 fs_id,置 0:此时 len==target,不会再有回填需求
            s.fs_id = new_id.unwrap_or(0);
            s.remote_size = s.target;
            s.len = s.target;
            s.dirty = false;
        }
        if old_id != 0 {
            self.dlink_cache.remove(&old_id);
        }
        if !parent.is_empty() {
            self.dir_cache.remove(&parent);
        }
        // 旧内容的读缓存块全部作废
        self.evict_blocks(ino);
    }

    /// 写会话文件的 attr:按 target 大小、当前时间报
    fn sess_attr(&self, req: &Request<'_>, ino: u64) -> FileAttr {
        let target = self.writes.get(&ino).map(|s| s.target).unwrap_or(0);
        let mut a = self.attr(req, ino, None);
        a.kind = FileType::RegularFile;
        a.size = target;
        a.blocks = target.div_ceil(512);
        a.perm = 0o644;
        a.nlink = 1;
        a
    }

    /// 读一个写会话中的文件:[0,len) 从暂存文件读,[len,remote_size) 回源拉,
    /// 再往后是虚段 0。不改会话状态(读不物化)
    fn read_session(&mut self, ino: u64, offset: u64, size: u32, reply: ReplyData) {
        let (path, fs_id, remote_size, len, tmp, target) = match self.writes.get(&ino) {
            Some(s) => (
                s.path.clone(),
                s.fs_id,
                s.remote_size,
                s.len,
                s.tmp.clone(),
                s.target,
            ),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if offset >= target {
            reply.data(&[]);
            return;
        }
        let n = (size as u64).min(target - offset);
        let mut buf = vec![0u8; n as usize];
        let end = offset + n;
        // 本地已物化部分
        let local = n.min(len.saturating_sub(offset));
        if local > 0 {
            let read_ok =
                std::fs::File::open(&tmp).and_then(|f| f.read_exact_at(&mut buf[..local as usize], offset));
            if let Err(e) = read_ok {
                tracing::warn!("读暂存 {} 失败:{e}", tmp.display());
                reply.error(libc::EIO);
                return;
            }
        }
        // 未物化但属于旧远端的部分
        let mut pos = offset.max(len);
        while pos < end.min(remote_size) {
            let step = (1u64 << 20).min(end.min(remote_size) - pos);
            match self.fetch_with_retry(fs_id, &path, "回填读", pos, step) {
                Ok(data) => {
                    if data.len() as u64 != step {
                        reply.error(libc::EIO);
                        return;
                    }
                    let at = (pos - offset) as usize;
                    buf[at..at + data.len()].copy_from_slice(&data);
                }
                Err(e) => {
                    reply.error(map_err(&e));
                    return;
                }
            }
            pos += step;
        }
        reply.data(&buf);
    }

    /// 丢弃写会话:删暂存文件,清引用计数
    fn drop_session(&mut self, ino: u64) {
        if let Some(s) = self.writes.remove(&ino) {
            let _ = std::fs::remove_file(&s.tmp);
        }
        self.write_refs.remove(&ino);
    }

    /// 淘汰某 inode 的全部读缓存块(上传覆盖/删除后旧内容作废)
    fn evict_blocks(&mut self, ino: u64) {
        let keys: Vec<(u64, u64)> = self
            .block_order
            .iter()
            .filter(|(i, _)| *i == ino)
            .copied()
            .collect();
        for k in keys {
            if let Some(v) = self.blocks.remove(&k) {
                self.cached_bytes -= v.len();
            }
        }
        self.block_order.retain(|(i, _)| *i != ino);
    }

    /// 删文件/删目录共用:先查类型给准确 errno,删完清一串本地状态
    fn remove_entry(&mut self, parent: u64, name: &OsStr, reply: ReplyEmpty, want_dir: bool) {
        let Some(parent_path) = self.path_of.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let name = name.to_string_lossy();
        let files = match self.list(&parent_path) {
            Ok(f) => f,
            Err(e) => {
                reply.error(map_err(&e));
                return;
            }
        };
        let Some(f) = files.into_iter().find(|f| f.name == name) else {
            reply.error(libc::ENOENT);
            return;
        };
        if f.is_dir != want_dir {
            reply.error(if want_dir { libc::ENOTDIR } else { libc::EISDIR });
            return;
        }
        if let Err(e) = self.client.delete(&f.path) {
            reply.error(map_err(&e));
            return;
        }
        self.dir_cache.remove(&parent_path);
        self.dlink_cache.remove(&f.fs_id);
        // 正在写的会话/读缓存一并清
        if let Some(ino) = self.ino_of.remove(&f.path) {
            self.path_of.remove(&ino);
            self.drop_session(ino);
            self.evict_blocks(ino);
        }
        reply.ok();
    }
}

/// 把 anyhow 错误映射成内核 errno:
/// -9 文件不存在 → ENOENT;-7 无权限 → EACCES;其余(网络/限频)→ EIO
fn map_err(e: &anyhow::Error) -> i32 {
    if let Some(api) = e.downcast_ref::<ApiError>() {
        match api.errno {
            -9 => return libc::ENOENT,
            -7 => return libc::EACCES,
            _ => {}
        }
    }
    tracing::warn!("FUSE 操作失败: {e:#}");
    libc::EIO
}

impl Filesystem for PanFs {
    /// 在父目录里找名字。内核每次路径解析都会来问,是调用最频繁的入口。
    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.path_of.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let name = name.to_string_lossy();
        // 写会话优先:新建/改过还没传回去的文件,远端列表里还没有
        let target = join_path(&parent_path, &name);
        if let Some(ino) = self
            .writes
            .iter()
            .find(|(_, s)| s.path == target)
            .map(|(ino, _)| *ino)
        {
            reply.entry(&TTL, &self.sess_attr(req, ino), 0);
            return;
        }

        match self.list(&parent_path) {
            Ok(files) => match files.iter().find(|f| f.name == name) {
                Some(f) => {
                    let ino = self.alloc_ino(&f.path);
                    let attr = self.attr(req, ino, Some(f));
                    reply.entry(&TTL, &attr, 0);
                }
                None => reply.error(libc::ENOENT),
            },
            Err(e) => reply.error(map_err(&e)),
        }
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.path_of.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        // 写会话中的文件:大小/时间以会话为准,不去问远端
        if self.writes.contains_key(&ino) {
            reply.attr(&TTL, &self.sess_attr(req, ino));
            return;
        }
        if ino == ROOT_INO {
            reply.attr(&TTL, &self.attr(req, ino, None));
            return;
        }
        match self.find(&path) {
            Ok(f) => reply.attr(&TTL, &self.attr(req, ino, Some(&f))),
            Err(e) => reply.error(map_err(&e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path_of.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let files = match self.list(&path) {
            Ok(f) => f,
            Err(e) => {
                reply.error(map_err(&e));
                return;
            }
        };

        // 拼出稳定的条目序列 "."、".."、子项,offset 是这个序列的下标。
        // 内核会带着递增的 offset 反复调用直到收到空应答。
        let parent_path = parent_of(&path);
        let dotdot_ino = *self
            .ino_of
            .get(&parent_path)
            .unwrap_or(&ROOT_INO);
        let mut entries: Vec<(u64, String, FileType)> = vec![
            (ino, ".".to_string(), FileType::Directory),
            (dotdot_ino, "..".to_string(), FileType::Directory),
        ];
        for f in &files {
            let child_ino = self.alloc_ino(&f.path);
            entries.push((
                child_ino,
                f.name.clone(),
                if f.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
            ));
        }

        let start = offset.max(0) as usize;
        for (i, (e_ino, e_name, e_kind)) in entries.iter().enumerate().skip(start) {
            // add 返回 false 表示内核缓冲满了,下次会从 i+1 继续
            if reply.add(*e_ino, (i + 1) as i64, *e_kind, e_name) {
                continue;
            }
            break;
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let acc = flags & libc::O_ACCMODE;
        if acc != libc::O_RDONLY && acc != libc::O_WRONLY && acc != libc::O_RDWR {
            reply.error(libc::EINVAL);
            return;
        }
        // 写句柄把"写"标志编进 fh(release 时据此归还引用);
        // 会话不在这建,首个 write/setattr 到来时惰性建
        if acc != libc::O_RDONLY {
            *self.write_refs.entry(ino).or_insert(0) += 1;
            reply.opened(1, 0);
        } else {
            reply.opened(0, 0);
        }
    }

    /// 读文件(三级策略):首读/seek 后直接拉请求窗口;连续顺序读升级成
    /// 16MB 块缓存;确认流式(≥2MB)后再放开预读 2 块,吞吐拉满。
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(path) = self.path_of.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        // 写会话中的文件走会话读(本地暂存 + 回源补洞),不碰远端元数据
        if self.writes.contains_key(&ino) {
            self.read_session(ino, offset.max(0) as u64, size, reply);
            return;
        }
        let f = match self.find(&path) {
            Ok(f) => f,
            Err(e) => {
                reply.error(map_err(&e));
                return;
            }
        };
        if f.is_dir {
            reply.error(libc::EISDIR);
            return;
        }

        let offset = offset.max(0) as u64;
        // 读到文件尾之后:返回空即 EOF
        if offset >= f.size {
            reply.data(&[]);
            return;
        }
        let n = (size as u64).min(f.size - offset);
        if n == 0 {
            reply.data(&[]);
            return;
        }

        // 顺序读判定 + 梯度:
        // - streak 0/1(首读、刚 seek):直接拉请求窗口,秒回,不整块不预读
        // - streak >= 2:走块缓存路径,但前 16 次(≈2MB)不预读,
        //   防止 seek 后的小读触发几十 MB 的预读
        // - streak >= 16:确认是真流式读,放开预读 2 块,吞吐拉满
        let sequential = match self.last_read {
            Some((lino, lend)) => {
                lino == ino
                    && offset >= lend.saturating_sub(64 * 1024)
                    && offset <= lend + SEQ_TOLERANCE
            }
            None => false,
        };
        self.seq_streak = if sequential { self.seq_streak.saturating_add(1) } else { 0 };
        self.last_read = Some((ino, offset + n));

        if self.seq_streak < 2 {
            match self.fetch_with_retry(f.fs_id, &path, "随机读", offset, n) {
                Ok(data) => {
                    if data.len() as u64 != n {
                        reply.error(libc::EIO);
                    } else {
                        reply.data(&data);
                    }
                }
                Err(e) => reply.error(map_err(&e)),
            }
            return;
        }

        // 块缓存 + 预读:把"服务已缓存块"和"拉取下一块"流水化,
        // 吞吐 ≈ 单块拉取速率(实测 16MB 块单连接 ~4-7MB/s,看 CDN 节点)
        let bs = self.block_size;
        let idx0 = offset / bs;
        let idx1 = (offset + n - 1) / bs;
        let last_block = (f.size - 1) / bs;
        let prefetch_end = if self.seq_streak >= 16 {
            (idx1 + 2).min(last_block)
        } else {
            idx1
        };

        for idx in idx0..=prefetch_end {
            if let Err(e) = self.ensure_block(ino, &path, f.fs_id, f.size, idx) {
                reply.error(map_err(&e));
                return;
            }
        }

        // 从缓存块里拼出要的数据
        let mut buf = Vec::with_capacity(n as usize);
        let mut pos = offset;
        while pos < offset + n {
            let idx = pos / bs;
            let b = self.blocks.get(&(ino, idx)).expect("块刚缓存过");
            let start = (pos - idx * bs) as usize;
            let take = ((offset + n) - pos).min((b.len() - start) as u64) as usize;
            buf.extend_from_slice(&b[start..start + take]);
            pos += take as u64;
        }
        reply.data(&buf);
    }

    /// 改属性:网盘只支持 truncate 这一种;其余(时间戳/权限)没对应语义,
    /// 回当前 attr 装作成功
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_of.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(new_size) = size else {
            if self.writes.contains_key(&ino) {
                reply.attr(&TTL, &self.sess_attr(req, ino));
            } else {
                match self.find(&path) {
                    Ok(f) => reply.attr(&TTL, &self.attr(req, ino, Some(&f))),
                    Err(e) => reply.error(map_err(&e)),
                }
            }
            return;
        };
        // truncate:没有会话就建一个(独立 truncate 命令的路径)
        if !self.writes.contains_key(&ino) {
            let (fs_id, remote_size) = match self.stat_remote(&path) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(map_err(&e));
                    return;
                }
            };
            // 远端没有这个文件:独立 truncate 对不存在的文件该报 ENOENT
            // (shell 的 `> 新文件` 走 create 钩子,不会到这)
            if fs_id == 0 {
                reply.error(libc::ENOENT);
                return;
            }
            if let Err(e) = self.open_write(ino, path.clone(), fs_id, remote_size) {
                reply.error(map_err(&e));
                return;
            }
        }
        {
            let Some(s) = self.writes.get_mut(&ino) else {
                reply.error(libc::EIO);
                return;
            };
            s.target = new_size;
            s.dirty = true;
            // 截短要真把暂存文件剪掉;截长靠虚段
            if new_size < s.len {
                if let Err(e) = s.file.set_len(new_size) {
                    tracing::warn!("截暂存失败:{e}");
                    reply.error(libc::EIO);
                    return;
                }
                s.len = new_size;
            }
        }
        // 没有任何写句柄打开(比如独立 `truncate` 命令):立即上传,
        // 不然等不来 flush/release
        if self.write_refs.get(&ino).copied().unwrap_or(0) == 0 {
            if let Err(e) = self.upload_session(ino) {
                reply.error(map_err(&e));
                return;
            }
        }
        reply.attr(&TTL, &self.sess_attr(req, ino));
    }

    /// 建文件节点:只支持普通文件;create 钩子没接住的路径会落到这
    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // 设备节点/管道等网盘没有对应概念
        if mode & libc::S_IFMT != libc::S_IFREG {
            reply.error(libc::EPERM);
            return;
        }
        let Some(parent_path) = self.path_of.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let path = join_path(&parent_path, &name.to_string_lossy());
        let ino = self.alloc_ino(&path);
        match self.stat_remote(&path) {
            Ok((fs_id, _)) => {
                if fs_id != 0 {
                    // 已存在:POSIX 语义是不动它
                    match self.find(&path) {
                        Ok(f) => reply.entry(&TTL, &self.attr(req, ino, Some(&f)), 0),
                        Err(e) => reply.error(map_err(&e)),
                    }
                    return;
                }
                // 新文件:mknod 不会跟 open/release,立即上传空文件
                if let Err(e) = self.open_write(ino, path, 0, 0) {
                    reply.error(map_err(&e));
                    return;
                }
                if let Some(s) = self.writes.get_mut(&ino) {
                    s.dirty = true;
                }
                if let Err(e) = self.upload_session(ino) {
                    tracing::warn!("mknod 上传失败:{e:#}");
                    reply.error(map_err(&e));
                    return;
                }
                reply.entry(&TTL, &self.sess_attr(req, ino), 0);
            }
            Err(e) => reply.error(map_err(&e)),
        }
    }

    /// 建目录:直接调网盘接口,成功后失效父目录缓存
    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_of.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let name = name.to_string_lossy();
        let path = join_path(&parent_path, &name);
        match self.client.mkdir(&path) {
            Ok(fs_id) => {
                self.dir_cache.remove(&parent_path);
                let ino = self.alloc_ino(&path);
                let mtime = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let nf = NetFile {
                    fs_id,
                    path,
                    name: name.to_string(),
                    is_dir: true,
                    size: 0,
                    mtime,
                };
                reply.entry(&TTL, &self.attr(req, ino, Some(&nf)), 0);
            }
            Err(e) => reply.error(map_err(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(parent, name, reply, false);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(parent, name, reply, true);
    }

    /// 改名/移动:统一走 filemanager move(dest=目标父目录 + 裸名 newname)。
    /// 目标已存在按 POSIX 先删;远端还没有源文件(建了没传)只改会话路径
    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(parent_path), Some(dest_dir)) = (
            self.path_of.get(&parent).cloned(),
            self.path_of.get(&newparent).cloned(),
        ) else {
            reply.error(libc::ENOENT);
            return;
        };
        let from = join_path(&parent_path, &name.to_string_lossy());
        let newname = newname.to_string_lossy();
        let to = join_path(&dest_dir, &newname);

        // 源还没传到远端(新建后未关句柄就被改名):纯本地改路径
        let remote_missing = self
            .ino_of
            .get(&from)
            .and_then(|&ino| self.writes.get(&ino))
            .is_some_and(|s| s.fs_id == 0);
        if remote_missing {
            if let Some(ino) = self.ino_of.remove(&from) {
                self.ino_of.insert(to.clone(), ino);
                self.path_of.insert(ino, to.clone());
                if let Some(s) = self.writes.get_mut(&ino) {
                    s.path = to.clone();
                }
            }
            self.dir_cache.remove(&parent_path);
            reply.ok();
            return;
        }

        // 目标已存在:POSIX 覆盖(文件盖文件);目标是目录不盖
        match self.list(&dest_dir) {
            Ok(files) => {
                if let Some(t) = files.into_iter().find(|f| f.name == newname) {
                    if t.is_dir {
                        reply.error(libc::EISDIR);
                        return;
                    }
                    if let Err(e) = self.client.delete(&t.path) {
                        reply.error(map_err(&e));
                        return;
                    }
                    self.dir_cache.remove(&dest_dir);
                    if let Some(ino) = self.ino_of.remove(&t.path) {
                        self.path_of.remove(&ino);
                        self.drop_session(ino);
                        self.evict_blocks(ino);
                    }
                }
            }
            Err(e) => {
                reply.error(map_err(&e));
                return;
            }
        }

        if let Err(e) = self.client.mv(&from, &dest_dir, &newname) {
            reply.error(map_err(&e));
            return;
        }
        self.dir_cache.remove(&parent_path);
        self.dir_cache.remove(&dest_dir);
        // 本地映射和写会话跟着搬
        if let Some(ino) = self.ino_of.remove(&from) {
            self.ino_of.insert(to.clone(), ino);
            self.path_of.insert(ino, to.clone());
            if let Some(s) = self.writes.get_mut(&ino) {
                s.path = to.clone();
            }
        }
        reply.ok();
    }

    /// 建并打开文件:查一下远端旧状态(覆盖写场景),开写会话
    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_path) = self.path_of.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let path = join_path(&parent_path, &name.to_string_lossy());
        let ino = self.alloc_ino(&path);
        let (fs_id, remote_size) = match self.stat_remote(&path) {
            Ok(v) => v,
            Err(e) => {
                reply.error(map_err(&e));
                return;
            }
        };
        if let Err(e) = self.open_write(ino, path, fs_id, remote_size) {
            reply.error(map_err(&e));
            return;
        }
        // 新建的文件也标 dirty:touch/O_CREAT 场景 close 时把空文件传上去
        if fs_id == 0 {
            if let Some(s) = self.writes.get_mut(&ino) {
                s.dirty = true;
            }
        }
        *self.write_refs.entry(ino).or_insert(0) += 1;
        // fh=1 标记写句柄(open 那套约定)
        reply.created(&TTL, &self.sess_attr(req, ino), 0, 1, 0);
    }

    /// 写文件:全落本地暂存,真正的上传发生在 flush/release/fsync
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let offset = offset.max(0) as u64;
        if !self.writes.contains_key(&ino) {
            if let Err(e) = self.ensure_session_for(ino) {
                reply.error(map_err(&e));
                return;
            }
        }
        // 写洞:先把 [len,offset) 物化成旧数据/0,避免之后回填盖掉这次写
        let gap = self.writes.get(&ino).is_some_and(|s| offset > s.len);
        if gap {
            if let Err(e) = self.backfill(ino, offset) {
                reply.error(map_err(&e));
                return;
            }
        }
        let Some(s) = self.writes.get_mut(&ino) else {
            reply.error(libc::EIO);
            return;
        };
        if let Err(e) = s
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| s.file.write_all(data))
        {
            tracing::warn!("写暂存 {} 失败:{e}", s.tmp.display());
            reply.error(libc::EIO);
            return;
        }
        let end = offset + data.len() as u64;
        s.len = s.len.max(end);
        s.target = s.target.max(end);
        s.dirty = true;
        reply.written(data.len() as u32);
    }

    /// close() 的报错窗口:dirty 就把整个文件传回去,让应用能看到错误
    fn flush(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if self.writes.get(&ino).is_some_and(|s| s.dirty) {
            match self.upload_session(ino) {
                Ok(()) => reply.ok(),
                Err(e) => {
                    tracing::warn!("flush 上传失败:{e:#}");
                    reply.error(map_err(&e));
                }
            }
        } else {
            reply.ok();
        }
    }

    /// 最后一个引用释放:补传 + 清会话。
    /// release 的错误应用看不到,失败时保留暂存文件供手动抢救
    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // fh=1 是写句柄(见 open/create)
        if fh == 1 {
            if let Some(n) = self.write_refs.get_mut(&ino) {
                *n = n.saturating_sub(1);
            }
            if self.write_refs.get(&ino).is_none_or(|n| *n == 0) {
                self.write_refs.remove(&ino);
                let mut ok = true;
                if self.writes.get(&ino).is_some_and(|s| s.dirty) {
                    match self.upload_session(ino) {
                        Ok(()) => {}
                        Err(e) => {
                            let tmp = self.writes.get(&ino).map(|s| s.tmp.clone());
                            tracing::error!(
                                "release 上传失败,暂存文件保留(可手动抢救):{tmp:?},原因:{e:#}"
                            );
                            ok = false;
                        }
                    }
                }
                if ok {
                    self.drop_session(ino);
                } else {
                    // 保留暂存文件,只清内存会话
                    self.writes.remove(&ino);
                }
            }
        }
        reply.ok();
    }

    /// 显式落盘语义:fsync 时就把该文件传回去
    fn fsync(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if self.writes.get(&ino).is_some_and(|s| s.dirty) {
            match self.upload_session(ino) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(map_err(&e)),
            }
        } else {
            reply.ok();
        }
    }

    /// df 用:总容量/已用映射到块统计
    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // 容量接口失败不影响挂载可用性,退化成全 0
        match self.client.quota() {
            Ok((total, used)) => {
                let blocks = total / 512;
                let free = total.saturating_sub(used) / 512;
                reply.statfs(blocks, free, free, 0, 0, 512, 255, 512);
            }
            Err(e) => {
                tracing::warn!("查询容量失败: {e:#}");
                reply.statfs(0, 0, 0, 0, 0, 512, 255, 512);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 路径拼接() {
        assert_eq!(join_path("/", "a"), "/a");
        assert_eq!(join_path("/apps/d", "x.txt"), "/apps/d/x.txt");
    }

    #[test]
    fn 父目录() {
        assert_eq!(parent_of("/"), "/");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("/apps/d/f.txt"), "/apps/d");
    }
}

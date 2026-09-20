//! 平台无关内核:FUSE 层(unix)与 Windows 按需同步层(win/)共用的
//! 路径工具、TTL 缓存、共享客户端(锁粒度可控)、带重试下载、三段式上传编排。
//!
//! 从 fs.rs 抽取,原则:这层不出现任何平台专属类型/导入,
//! fuser 概念(inode/Reply*)和 cldapi 概念(placeholder/pin)都不进这里。

use crate::baidu::{self, upload_slice_size, BaiduClient, NetFile};
use crate::progress::Progress;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
#[cfg(windows)] // remote_to_local 的返回类型,unix 侧不用
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------- 远端路径工具 ----------

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

/// 规范化远端根:必须以 / 开头,去掉末尾 /(根目录保留 "/")
pub fn normalize_root(root: &str) -> String {
    let mut r = root.trim_end_matches('/').to_string();
    if !r.starts_with('/') {
        r = format!("/{r}");
    }
    if r.is_empty() {
        r = "/".to_string();
    }
    r
}

/// 本地同步根下的路径 → 远端绝对路径(相对部分同构挂在 root 下)。
/// 返回 None:local 不在 sync_root 下。Windows 磨掉路径大小写差异。
#[cfg(windows)]
pub fn local_to_remote(local: &Path, sync_root: &Path, root: &str) -> Option<String> {
    let rel = rel_under(local, sync_root)?;
    let root = normalize_root(root);
    if rel.is_empty() {
        Some(root)
    } else {
        Some(join_path(&root, &rel))
    }
}

/// 远端绝对路径 → 本地同步根下的路径(root 之下的相对部分同构映射回去)
#[cfg(windows)]
pub fn remote_to_local(remote: &str, root: &str, sync_root: &Path) -> PathBuf {
    let root = normalize_root(root);
    let rel = remote.strip_prefix(&root).unwrap_or(remote).trim_start_matches('/');
    let mut p = sync_root.to_path_buf();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

/// local 相对 sync_root 的部分(统一 / 分隔,根本身返回空串)。
/// 前缀比较忽略大小写(Windows 文件系统不区分),但保留原始大小写返回
#[cfg(windows)]
fn rel_under(local: &Path, sync_root: &Path) -> Option<String> {
    let l = local.to_string_lossy().replace('\\', "/");
    let r = sync_root.to_string_lossy().replace('\\', "/");
    let r = r.trim_end_matches('/');
    if l.eq_ignore_ascii_case(r) {
        return Some(String::new());
    }
    // starts_with + 忽略大小写:eq_ignore_ascii_case 保证等长,字节切安全
    if l.len() > r.len() + 1 && l[..r.len()].eq_ignore_ascii_case(r) && l.as_bytes()[r.len()] == b'/' {
        return Some(l[r.len() + 1..].to_string());
    }
    None
}

// ---------- TTL 缓存 ----------

/// 目录列表缓存(带 TTL)。内核/Explorer 的一次目录浏览会触发几十次元数据
/// 查询,而未过审应用 10 次/小时——不打缓存配额秒光,这是生命线。
pub struct DirCache {
    ttl: Duration,
    inner: Mutex<HashMap<String, (Vec<NetFile>, Instant)>>,
}

impl DirCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs.max(1)),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 命中直接回;未命中拉一次(客户端锁只在 API 调用期间持有)。
    /// 并发未命中可能重复拉取,配额场景下概率低,可接受
    pub fn get_or_fetch(&self, client: &SharedClient, dir: &str) -> Result<Vec<NetFile>> {
        {
            let m = self.lock();
            if let Some((files, at)) = m.get(dir) {
                if at.elapsed() < self.ttl {
                    return Ok(files.clone());
                }
            }
        }
        let files = client.with(|c| c.list_dir(dir))?;
        tracing::debug!("拉取目录 {dir}: {} 条", files.len());
        self.lock().insert(dir.to_string(), (files.clone(), Instant::now()));
        Ok(files)
    }

    /// 拿一份可能过期的旧值:配额报错时兜底,别让已看过的目录整个消失
    /// (仅 Windows 按需枚举用;FUSE 路径配额报错直接向上抛)
    #[cfg(windows)]
    pub fn stale(&self, dir: &str) -> Option<Vec<NetFile>> {
        self.lock().get(dir).map(|(f, _)| f.clone())
    }

    pub fn remove(&self, dir: &str) {
        self.lock().remove(dir);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Vec<NetFile>, Instant)>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 下载直链缓存(带 TTL)。dlink 官方 8h 有效,保守 30 分钟;
/// 实测顺序复用同一 dlink 没问题,之前的 403 是并发波浪触发的
pub struct DLinkCache {
    ttl: Duration,
    inner: Mutex<HashMap<u64, (String, Instant)>>,
}

impl DLinkCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs.max(1)),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 过期或没有都返回 None(重新获取的决定在调用方,配合 403 重试)
    pub fn get(&self, fs_id: u64) -> Option<String> {
        let m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.get(&fs_id)
            .filter(|(_, at)| at.elapsed() < self.ttl)
            .map(|(d, _)| d.clone())
    }

    pub fn put(&self, fs_id: u64, dlink: String) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fs_id, (dlink, Instant::now()));
    }

    pub fn remove(&self, fs_id: u64) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&fs_id);
    }
}

// ---------- 共享客户端 ----------

/// 并发安全的客户端包装。cldapi 回调在 OS 线程池并发进来(FUSE 是单线程
/// 分发,不受影响):元数据/变更操作走短临界区 with();下载先在锁内拿
/// download_handle() 快照,放锁后再传输——网络传输不占客户端锁。
pub struct SharedClient {
    inner: Mutex<BaiduClient>,
}

impl SharedClient {
    pub fn new(c: BaiduClient) -> Self {
        Self {
            inner: Mutex::new(c),
        }
    }

    /// 短临界区操作(token 自愈/刷新都在锁内安全)
    pub fn with<R>(&self, f: impl FnOnce(&mut BaiduClient) -> R) -> R {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    /// 统一的带重试拉取:403(dlink 失效/被限)时弃缓存换新链再来,
    /// 最多 2 次,带递增退避。path/kind 只用于进度展示。
    /// 传输全程不持客户端锁
    pub fn fetch_range(
        &self,
        dlinks: &DLinkCache,
        fs_id: u64,
        path: &str,
        kind: &str,
        off: u64,
        len: u64,
        parallel: usize,
        prog: &Progress,
    ) -> Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            let dlink = match dlinks.get(fs_id) {
                Some(d) => d,
                None => {
                    let d = self.with(|c| c.get_dlink(fs_id))?;
                    dlinks.put(fs_id, d.clone());
                    d
                }
            };
            let (http, token) = self.with(|c| c.download_handle());
            let id = prog.begin(path, kind, off, len);
            let r = baidu::read_range_with(&http, &token, &dlink, off, len, parallel, prog, id);
            prog.end(id, r.is_ok());
            match r {
                Ok(d) => return Ok(d),
                Err(e) if attempt < 2 && baidu::is_forbidden(&e) => {
                    attempt += 1;
                    // 403 多半意味着这条 dlink 已被限/失效,弃缓存下次换新链
                    dlinks.remove(fs_id);
                    tracing::warn!("下载 403,弃 dlink 缓存换新链重试(第 {attempt} 次)");
                    std::thread::sleep(Duration::from_millis(300 * attempt as u64));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// ---------- 三段式上传编排 ----------

/// 把一个本地文件三段式传回网盘(precreate → superfile2 分片 → create)。
/// 读文件用可移植的 seek+read,unix/Windows 通用。
/// 返回新 fs_id;秒传(precreate 命中网盘已有内容)拿不到 fs_id,返回 None。
/// 空 block_list=[] 会被 precreate 拒(errno=2)——用 [空串md5] 当唯一分片,
/// 实测 precreate 直接回 need=[] 免传,create 收尾。
pub fn upload_local_file(
    client: &SharedClient,
    prog: &Progress,
    local: &Path,
    remote: &str,
    size: u64,
) -> Result<Option<u64>> {
    // 分片规则:4MB 起步,>4GB 自动放大,保证 ≤1024 片(官方上限)
    let slice = upload_slice_size(size);
    let nblocks = size.div_ceil(slice);
    let mut f = std::fs::File::open(local)?;
    let mut buf = vec![0u8; slice as usize];
    let mut md5s: Vec<String> = Vec::with_capacity(nblocks as usize);
    if nblocks == 0 {
        md5s.push(format!("{:x}", md5::compute(&[])));
    }
    for i in 0..nblocks {
        let want = slice.min(size - i * slice) as usize;
        read_exact_at(&mut f, &mut buf[..want], i * slice)?;
        md5s.push(format!("{:x}", md5::compute(&buf[..want])));
    }

    let new_id = match client.with(|c| c.precreate(remote, size, &md5s))? {
        None => None, // 秒传
        Some((uploadid, need)) => {
            for seq in need {
                if seq >= nblocks {
                    bail!("precreate 要分片#{seq},本地只有 {nblocks} 片");
                }
                let off = seq * slice;
                let want = slice.min(size - off) as usize;
                let mut data = vec![0u8; want];
                read_exact_at(&mut f, &mut data, off)?;
                client.with(|c| c.upload_slice(&uploadid, remote, seq, &data, prog))?;
            }
            Some(client.with(|c| c.create_file(remote, size, &uploadid, &md5s))?)
        }
    };
    tracing::info!("上传完成:{remote}({size} 字节,{nblocks} 片)");
    Ok(new_id)
}

/// 便携的定位读(等价 unix FileExt::read_exact_at;std 无跨平台版本)
pub(crate) fn read_exact_at(f: &mut std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
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

    #[test]
    fn 根规范化() {
        assert_eq!(normalize_root("/"), "/");
        assert_eq!(normalize_root(""), "/");
        assert_eq!(normalize_root("apps/x"), "/apps/x");
        assert_eq!(normalize_root("/apps/x/"), "/apps/x");
    }

    #[test]
    fn 本地远端路径映射() {
        let root = Path::new(r"C:\Users\u\BaiduNetdisk");
        assert_eq!(
            local_to_remote(Path::new(r"C:\Users\u\BaiduNetdisk\a\b.txt"), root, "/"),
            Some("/a/b.txt".to_string())
        );
        assert_eq!(
            local_to_remote(
                Path::new(r"c:\users\u\baidunetdisk\a\B.TXT"),
                root,
                "/apps/x"
            ),
            Some("/apps/x/a/B.TXT".to_string())
        );
        // 根本身 → 远端根
        assert_eq!(local_to_remote(root, root, "/apps/x"), Some("/apps/x".to_string()));
        // 根外 → None
        assert_eq!(
            local_to_remote(Path::new(r"C:\Users\u\Desktop\f.txt"), root, "/"),
            None
        );

        // 期望值用 join 构造:PathBuf 拼接符随平台(/ vs \),断言才不偏科
        assert_eq!(
            remote_to_local("/apps/x/a/b.txt", "/apps/x", root),
            root.join("a").join("b.txt")
        );
        assert_eq!(remote_to_local("/", "/", root), root.to_path_buf());
    }
}

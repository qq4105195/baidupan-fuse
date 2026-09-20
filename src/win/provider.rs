//! WinProvider:cloud_filter 回调实现。
//! M2 只实现目录枚举(fetch_placeholders)——Explorer 展开目录时把远端
//! 条目建成云图标占位符;水合(M3)和回传(M4)后续里程碑接入。

use crate::baidu::NetFile;
use crate::core::{self, DirCache, DLinkCache, SharedClient};
use crate::progress::Progress;
use crate::settings::Settings;
use cloud_filter::error::{CloudErrorKind, CResult};
use cloud_filter::filter::{self, ticket, Request, SyncFilter};
use cloud_filter::metadata::{Metadata, MetadataExt};
use cloud_filter::placeholder_file::PlaceholderFile;
use std::collections::HashSet;
use std::path::PathBuf;

/// 同步提供方:挂在 Explorer 里的一棵"百度网盘"树
pub struct WinProvider {
    /// 共享客户端(锁粒度见 core::SharedClient)
    pub(crate) client: SharedClient,
    /// 目录列表缓存(Windows 侧建议 dir_ttl=300:枚举风暴打不起配额)
    dir_cache: DirCache,
    /// dlink 缓存(M3 水合用;先建好)
    pub(crate) dlink_cache: DLinkCache,
    /// 远端根(网盘里的哪棵子树映射到同步根)
    root: String,
    /// 本地同步根绝对路径
    pub(crate) sync_root: PathBuf,
    /// 下载/上传进度(progress.json)
    pub(crate) progress: Progress,
}

impl WinProvider {
    pub fn new(st: &Settings, sync_root: PathBuf) -> anyhow::Result<Self> {
        let client = crate::baidu::BaiduClient::from_config()?;
        Ok(Self {
            client: SharedClient::new(client),
            dir_cache: DirCache::new(st.dir_ttl),
            dlink_cache: DLinkCache::new(st.dlink_ttl),
            root: core::normalize_root(&st.root),
            sync_root,
            progress: Progress::new(),
        })
    }
}

impl SyncFilter for WinProvider {
    /// M3 实现(1MB 分块流式下载 + 进度);现在双击打开会报错,是本里程碑预期行为。
    /// 注意:不能直接返回 Err——上游 proxy 同步发的 Write::fail 填的 Offset/Length
    /// 全 0,会被平台拒绝并 unwrap 杀进程(vendor/cloud-filter/PATCHES.md);
    /// 正确姿势是带 required range 自己发失败应答,然后返回 Ok
    fn fetch_data(
        &self,
        _request: Request,
        ticket: ticket::FetchData,
        info: filter::info::FetchData,
    ) -> CResult<()> {
        tracing::warn!("fetch_data:水合还没实现(M3)");
        if let Err(e) = ticket.fail(CloudErrorKind::Unsuccessful, info.required_file_range()) {
            tracing::warn!("失败应答未送达(平台将按超时处理):{e}");
        }
        Ok(())
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

        // 本地已有同名条目(占位符或真实文件)跳过:重展开只补缺,不覆盖
        let existing = list_local_names(&dir);
        let mut phs: Vec<PlaceholderFile> = Vec::with_capacity(files.len());
        for f in &files {
            if !win_name_ok(&f.name) {
                tracing::warn!("跳过 Windows 非法名:{}", f.path);
                continue;
            }
            if existing.contains(&f.name.to_lowercase()) {
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

/// 目录下已有的名字(小写,匹配用);读不了当空集
fn list_local_names(dir: &std::path::Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_lowercase())
                .collect()
        })
        .unwrap_or_default()
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

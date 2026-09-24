//! 多网盘抽象:各网盘客户端共同实现的后端接口。
//!
//! fs.rs 只认这里的 NetFile / PanError / PanClient,不关心底下是百度还是联通:
//! - 百度:路径寻址、数字 fs_id、precreate 三段式上传 —— 都藏在 impl 内部
//! - 联通(wopan):目录 id 寻址、字符串双 id(id/fid)、8MB 分片直传 —— 同样藏在 impl 内部
//!
//! trait 表面统一用路径 + 字符串 id,fs.rs 的 ino↔path 表和缓存逻辑全后端共用。

pub mod baidu;
pub mod wopan;

use crate::progress::Progress;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------- 后端种类 ----------

/// 支持的网盘后端。配置目录里的文件名、systemd 服务名、进度文件名都按它区分
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanKind {
    Baidu,
    Wopan,
}

impl PanKind {
    /// 配置/CLI 用的短名
    pub fn id(&self) -> &'static str {
        match self {
            PanKind::Baidu => "baidu",
            PanKind::Wopan => "wopan",
        }
    }

    /// 人读名(菜单/日志用)
    pub fn label(&self) -> &'static str {
        match self {
            PanKind::Baidu => "百度网盘",
            PanKind::Wopan => "联通云盘",
        }
    }

    pub fn all() -> [PanKind; 2] {
        [PanKind::Baidu, PanKind::Wopan]
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "baidu" | "百度" => Some(PanKind::Baidu),
            "wopan" | "wo" | "联通" | "联通云盘" => Some(PanKind::Wopan),
            _ => None,
        }
    }
}

/// 从本地凭据构建对应后端的客户端(token 自动续期/校验)
pub fn client_for(kind: PanKind) -> Result<Box<dyn PanClient>> {
    match kind {
        PanKind::Baidu => Ok(Box::new(baidu::BaiduClient::from_config()?)),
        PanKind::Wopan => Ok(Box::new(wopan::WopanClient::from_config()?)),
    }
}

/// 各后端共用的配置根目录:~/.config/baidupan-fuse/
/// (项目起家于百度,目录名沿用;每个后端的凭据/暂存/进度文件在下面按名字区分)
pub fn config_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("baidupan-fuse")
}

// ---------- 文件模型 ----------

/// 网盘里的一个文件/目录条目(各后端的原始模型都归一成这个)
#[derive(Clone, Debug)]
pub struct NetFile {
    /// 后端条目标识。百度:fs_id 十进制串;wopan:32 位 hex 条目 id。
    /// fs 层拿它当 dlink 缓存的 key;空串 = 远端还没有这个文件(新建未上传)
    pub id: String,
    /// 下载用标识。百度与 id 相同(其实不用它);wopan 是内容实体 fid
    pub fid: String,
    /// 服务端绝对路径,如 /apps/demo/a.txt(fs 层的路径协议靠它)
    pub path: String,
    /// 显示名
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间,unix 秒(各后端原始格式在客户端层归一)
    pub mtime: i64,
}

// ---------- 错误 ----------

/// 业务错误归一:fs 层 match 这个决定回内核哪个 errno、要不要重试
#[derive(Debug)]
pub enum PanError {
    /// 文件/目录不存在 → ENOENT
    NotFound,
    /// 无权限(含"目标是目录"这类语义冲突)→ EACCES/相关
    PermissionDenied,
    /// token 失效且刷新重试后仍失败
    AuthExpired,
    /// 下载直链被 CDN 拒(403):多半是链接过期/被限,重取直链可自愈
    LinkExpired,
    /// 限流(429/503),稍后重试
    RateLimited,
    /// 其余业务错误,带描述
    Api(String),
}

impl std::fmt::Display for PanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PanError::NotFound => write!(f, "文件不存在"),
            PanError::PermissionDenied => write!(f, "没有权限"),
            PanError::AuthExpired => write!(f, "登录已失效(刷新后仍失败),请重新登录"),
            PanError::LinkExpired => write!(f, "下载直链已失效"),
            PanError::RateLimited => write!(f, "被限流,稍后再试"),
            PanError::Api(m) => write!(f, "网盘 API 错误:{m}"),
        }
    }
}

impl std::error::Error for PanError {}

// ---------- 后端接口 ----------

/// 一个网盘后端。fs.rs(PanFs)只通过这组方法访问网盘。
/// 方法都是 &mut self:fuser 单线程分发,串行调用,天然无并发问题
/// (read_range 例外,它内部自己开线程并发拉数据)。
pub trait PanClient: Send {
    /// 列目录(后端内部负责翻页拉全)。返回的 NetFile.path 是完整路径
    fn list_dir(&mut self, dir: &str) -> Result<Vec<NetFile>>;

    /// 拿下载直链(短时效,fs 层缓存 + 失效重取)
    fn get_dlink(&mut self, f: &NetFile) -> Result<String>;

    /// 拉一段数据:parts 份并行 Range 请求按序拼接。
    /// prog/id 用于实时进度。403/限流要归一成 PanError::LinkExpired/RateLimited
    fn read_range(
        &self,
        url: &str,
        offset: u64,
        len: u64,
        parts: usize,
        prog: &Progress,
        id: u64,
    ) -> Result<Vec<u8>>;

    /// 整文件上传(后端内部自行分段:百度 precreate 三段式+秒传,
    /// wopan 8MB 分片直传)。src 已补齐洞、可从 0 顺序读 size 字节。
    /// 返回远端新文件(秒传拿不到标识时 id 为空串)
    fn upload(
        &mut self,
        path: &str,
        src: &mut std::fs::File,
        size: u64,
        prog: &Progress,
    ) -> Result<NetFile>;

    /// 建目录,返回新目录
    fn mkdir(&mut self, path: &str) -> Result<NetFile>;

    /// 删除文件/目录(通常进网盘回收站)
    fn delete(&mut self, f: &NetFile) -> Result<()>;

    /// 改名/移动:f 是源条目,dest_dir 是目标目录路径,newname 是新名
    fn mv(&mut self, f: &NetFile, dest_dir: &str, newname: &str) -> Result<()>;

    /// 容量,返回 (总字节, 已用字节)
    fn quota(&mut self) -> Result<(u64, u64)>;

    /// 账号展示名(菜单/CLI info 用,含账号与会员信息的一行字)
    fn account(&mut self) -> Result<String>;

    /// 后端种类(暂存目录/进度文件名用)
    fn kind(&self) -> PanKind;
}

/// 该后端的写暂存目录(config 目录下,按后端隔离;挂载时清空重建)
pub fn staging_dir(kind: PanKind) -> PathBuf {
    config_dir().join(format!("uploads-{}", kind.id()))
}

/// 该后端的进度文件(挂载进程写、控制台读)
pub fn progress_file(kind: PanKind) -> PathBuf {
    config_dir().join(format!("progress-{}.json", kind.id()))
}

// ---------- token 持久化的小工具(两个后端共用形状) ----------

/// 通用 token 落盘:原子写(tmp + rename),凭据文件别留半截
pub fn save_json_atomic(path: &std::path::Path, v: &impl Serialize) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 读 JSON 凭据文件,顺带把错误包装成"请先登录"的提示
pub fn load_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读 {path:?} 失败({e}),请先运行 bdfs --backend … login"))?;
    Ok(serde_json::from_str(&raw)?)
}

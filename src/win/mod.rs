//! Windows 模式:把百度网盘做成 OneDrive 式按需文件夹(Cloud Files API / cldapi)。
//!
//! 形态:注册一个同步根目录(默认 %USERPROFILE%\BaiduNetdisk),里面的文件是
//! 云图标占位符;打开时按需水合(下载),右键"始终保留在此设备"= pin 全量落地,
//! "释放空间"= 脱水回占位符;本地增删改改名回传网盘(后台队列)。
//!
//! 模块布局:
//! - mod.rs        同步根注册/注销、生命周期 run()、单实例与停止信号
//! - identity.rs   占位符身份 blob 编解码(fs_id/is_dir/size/mtime,≤4KB)
//! - provider.rs   WinProvider:cloud_filter SyncFilter 回调实现
//! - hydrate.rs    fetch_data 水合管线(分块下载 + 取消)
//! - syncback.rs   本地变更回传队列(防抖/重试/自触抑制)

use crate::settings::Settings;

/// 同步根提供方标识(注册进系统 SyncRootManager,格式 bdfs!<用户SID>!<账号>)
pub const PROVIDER: &str = "bdfs";

/// 同步根是否已注册(Explorer 是否已认下这个提供方)
pub fn is_registered(sync_root: &str) -> bool {
    cloud_filter::root::SyncRootId::from_path(sync_root)
        .and_then(|id| id.is_registered())
        .unwrap_or(false)
}

/// 启动同步提供方:注册同步根 → 连接回调 → 前台运行到 Ctrl-C/停止信号。
pub fn run(st: &Settings) -> anyhow::Result<()> {
    let _ = st;
    anyhow::bail!("按需同步还在开发中;当前请用 Linux/macOS FUSE 版或路由器 Samba")
}

/// 通知正在跑的同步进程退出;返回是否有进程被通知。
pub fn stop(_sync_root: &str) -> bool {
    // M5:OpenEventW("Local\\bdfs-sync-stop") + SetEvent;M0 先恒 false
    false
}

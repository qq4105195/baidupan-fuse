//! Windows 模式:把百度网盘做成 OneDrive 式按需文件夹(Cloud Files API / cldapi)。
//!
//! 形态:注册一个同步根目录(默认 %USERPROFILE%\BaiduNetdisk),里面的文件是
//! 云图标占位符;打开时按需水合(下载),右键"始终保留在此设备"= pin 全量落地,
//! "释放空间"= 脱水回占位符;本地增删改改名回传网盘(后台队列)。
//!
//! 模块布局:
//! - mod.rs        同步根注册/注销(幂等)、生命周期 run()、停止信号
//! - identity.rs   占位符身份 blob 编解码(fs_id/is_dir/size/mtime,≤4KB)
//! - provider.rs   WinProvider:cloud_filter SyncFilter 回调实现
//! - hydrate.rs    fetch_data 水合管线(分块下载 + 取消)(M3)
//! - syncback.rs   本地变更回传队列(防抖/重试/自触抑制)(M4)

mod hydrate;
mod identity;
mod provider;
mod syncback;

use crate::settings::Settings;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// 同步根提供方标识(SyncRootId 的第一段,格式 bdfs!<用户SID>!<账号>)
pub const PROVIDER: &str = "bdfs";

/// 同步根是否已注册(Explorer 是否已认下这个提供方)
pub fn is_registered(sync_root: &str) -> bool {
    let p = resolve_sync_root(sync_root);
    cloud_filter::root::SyncRootId::from_path(&p)
        .and_then(|id| id.is_registered())
        .unwrap_or(false)
}

/// 启动按需同步:检查支持 → 建目录 → 注册(幂等)→ 连接回调 → 前台跑到 Ctrl-C。
/// 退出时只断开连接、不注销:占位符在离线状态仍可见(OneDrive 同款行为)
pub fn run(st: &Settings) -> Result<()> {
    let supported = cloud_filter::root::is_supported().map_err(|e| anyhow!("检查 Cloud Files API:{e}"))?;
    if !supported {
        anyhow::bail!("这台 Windows 不支持 Cloud Files API(需 Win10 1709+ / NTFS)");
    }
    let sync_root = resolve_sync_root(&st.sync_root);
    std::fs::create_dir_all(&sync_root)?;
    register(&sync_root)?;
    println!("同步根:{}(远端根 {})", sync_root.display(), st.root);

    let prov = provider::WinProvider::new(st, sync_root.clone())?;
    // 启动扫(都在连接后异步动工——update/convert 是 provider-only 调用,
    // 连接本身毫秒级,线程先睡 1s 兜底):
    // 1) 修复上次进程暴死留下的撕裂目录 2) 脏扫补传断网改动/没传完的
    prov.syncback.scan_after_connect(sync_root.clone());
    let repair_root = sync_root.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        provider::repair_torn_dirs(&repair_root);
    });
    // 注意:不能开 block_implicit_hydration——实测它把 pin("始终保留在此设备")
    // 触发的后台水合也拦了(13s 无回调),杀软误触发下载的代价认了
    let connection = cloud_filter::root::Session::new()
        .connect(&sync_root, prov)
        .map_err(|e| anyhow!("连接同步根失败:{e}"))?;
    println!("按需同步运行中(双击/右键\"始终保留在此设备\"触发下载)。Ctrl-C 退出。");

    // Ctrl-C / 控制台关闭 → 收到信号断开;ctrlc 在 Windows 上走 SetConsoleCtrlHandler
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .map_err(|e| anyhow!("装 Ctrl-C 处理器失败:{e}"))?;
    let _ = rx.recv();
    println!("正在断开(不注销,占位符保持可见)…");
    drop(connection);
    Ok(())
}

/// 注册同步根(幂等:已注册直接返回)。策略:
/// Hydration::Full(打开=整文件下载)+ Population::Full(展开目录时才回调填充)
/// + 允许 pin("始终保留在此设备"/"释放空间"两个右键项)
pub fn register(sync_root: &Path) -> Result<()> {
    let id = sync_root_id()?;
    if id.is_registered().map_err(|e| anyhow!("查询注册状态:{e}"))? {
        tracing::info!("同步根已注册,跳过");
        return Ok(());
    }
    use cloud_filter::root::{HydrationType, PopulationType, SupportedAttribute, SyncRootInfo};
    let info = SyncRootInfo::default()
        .with_path(sync_root)
        .map_err(|e| anyhow!("同步根路径不可用:{e}"))?
        .with_display_name("百度网盘")
        // 图标资源串:先借系统的云图标,Explorer 不识别就回落默认文件夹图标
        .with_icon(r"%SystemRoot%\System32\imageres.dll,-1043")
        .with_version("1.0.0")
        .with_hydration_type(HydrationType::Full)
        .with_population_type(PopulationType::Full)
        .with_allow_pinning(true)
        // InSyncPolicy 声明"哪些本地改动算把文件弄脏":内容/时间/属性全算。
        // 内核据此自动把 in-sync 清掉,M3 的脱水拒绝、M4 的脏文件回传都靠它
        .with_supported_attribute(
            SupportedAttribute::FileSystem
                | SupportedAttribute::FileCreationTime
                | SupportedAttribute::FileLastWriteTime
                | SupportedAttribute::FileReadonly
                | SupportedAttribute::FileHidden
                | SupportedAttribute::DirectoryCreationTime
                | SupportedAttribute::DirectoryLastWriteTime
                | SupportedAttribute::DirectoryReadonly
                | SupportedAttribute::DirectoryHidden,
        );
    id.register(info)
        .map_err(|e| anyhow!("注册同步根失败:{e}"))?;
    println!("已注册同步根(Explorer 侧边栏会出现「百度网盘」)");
    Ok(())
}

/// 注销同步根(菜单兜底用:进程崩了残留注册、或不想用了)。
/// 已注销时按成功处理(幂等;0x80070490 = ERROR_NOT_FOUND)
pub fn unregister() -> Result<()> {
    match sync_root_id()?.unregister() {
        Ok(()) => {
            println!("已注销同步根(本地文件保留为普通文件/占位符)");
            Ok(())
        }
        Err(e) if e.code().0 == 0x8007_0490u32 as i32 => {
            println!("同步根本就未注册,视为已注销");
            Ok(())
        }
        Err(e) => Err(anyhow!("注销失败:{e}")),
    }
}

/// 同步根 ID:bdfs!<当前用户SID>!<账号名>。
/// 账号名暂用固定串(v1 单账号机器;换账号 = 先注销再重登)
fn sync_root_id() -> Result<cloud_filter::root::SyncRootId> {
    use cloud_filter::root::{SecurityId, SyncRootIdBuilder};
    Ok(SyncRootIdBuilder::new(PROVIDER)
        .user_security_id(
            SecurityId::current_user().map_err(|e| anyhow!("取当前用户 SID:{e}"))?,
        )
        .account_name("baidu")
        .build())
}

/// 把设置里的 sync_root 解析成绝对路径:相对名按用户主目录(%USERPROFILE%)
pub fn resolve_sync_root(s: &str) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(p)
    }
}

/// 通知正在跑的同步进程退出;返回是否有进程被通知。
pub fn stop(_sync_root: &str) -> bool {
    // M5:命名事件 Local\bdfs-sync-stop(注册自启 + 单实例一起做);当前恒 false
    false
}

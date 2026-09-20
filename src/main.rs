//! bdfs:百度网盘挂载/同步工具。
//! - Linux/macOS:FUSE 挂载(fuser,读 + 写,close 时三段式上传)
//! - Windows:OneDrive 式按需文件夹(Cloud Files API,云图标占位/打开即下载/
//!   始终保留在此设备,见 src/win/)
//! 裸跑(不带子命令)进交互控制台,子命令供脚本使用。

mod baidu;
mod core;
#[cfg(unix)]
mod fs;
mod menu;
mod progress;
mod settings;
#[cfg(windows)]
mod win;

use anyhow::Result;
use clap::{Parser, Subcommand};
#[cfg(unix)]
use fuser::MountOption;
#[cfg(unix)]
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "bdfs",
    version,
    about = "百度网盘挂载:FUSE(Linux/macOS)/ 按需文件夹(Windows);不带子命令进入交互控制台"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 设备码模式登录:保存 app 凭据和 OAuth token
    Login {
        /// 开放平台应用的 AppKey(client_id)
        #[arg(short = 'k', long)]
        app_key: String,
        /// 开放平台应用的 SecretKey(client_secret)
        #[arg(short = 's', long)]
        app_secret: String,
        /// 跳过设备码,直接用授权码模式(应用没开通设备码授权时用)
        #[arg(long)]
        code_mode: bool,
    },
    /// 显示用户信息和网盘容量(验证登录是否有效)
    Info,
    /// 列远端目录(不挂载,冒烟测试用)
    Ls {
        /// 远端目录,默认根目录
        path: String,
    },
    /// 启动 Windows 按需同步(前台运行,自动注册同步根,Ctrl-C 退出)
    #[cfg(windows)]
    Sync {
        /// 同步根目录(默认 %USERPROFILE%\BaiduNetdisk)
        sync_root: Option<String>,
    },
    /// 启动系统托盘(右下角图标:开关同步/自启/设置/进度;自动带起同步,日常入口)。
    /// 双击桌面图标 = 打开网盘文件夹(托盘没跑就顺手带起);--quiet 供开机自启,
    /// 只起托盘不弹文件夹
    #[cfg(windows)]
    Tray {
        /// 静默启动(开机自启用):不弹资源管理器窗口
        #[arg(long)]
        quiet: bool,
    },
    /// 打开设置窗口(托盘「设置…」弹的就是它;保存后同步自动重启生效)
    #[cfg(windows)]
    Config,
    /// 显示传输进度,按回车退出(托盘「传输进度…」用;也可手动跑)
    #[cfg(windows)]
    Progress,
    /// 注销 Windows 同步根(Explorer 恢复普通文件夹;本地文件保留)
    #[cfg(windows)]
    Unregister,
    /// 挂载到本地目录(Ctrl-C 退出后用 fusermount -u 卸载)
    #[cfg(unix)]
    Mount {
        /// 本地挂载点
        mountpoint: PathBuf,
        /// 挂载的远端根目录;未过审应用只能访问 /apps/<应用名>
        #[arg(short = 'r', long, default_value = "/")]
        root: String,
        /// 目录列表缓存秒数(默认 60,调大省 API 配额)
        #[arg(long, default_value_t = 60)]
        dir_ttl: u64,
        /// 下载直链缓存秒数(官方 8 小时有效,默认保守 1800)
        #[arg(long, default_value_t = 1800)]
        dlink_ttl: u64,
        /// 顺序读块大小 MB:按块拉取+缓存,内核 128KB 小读全部命中缓存。
        /// 实测块越大吞吐越高(连接爬坡摊薄):8MB≈4MB/s,16MB≈7MB/s
        #[arg(long, default_value_t = 16)]
        block_mb: u64,
        /// 每块并发连接数。实测 SVIP 账号单长流才是高速通道,
        /// 并发波浪会被 CDN 限速,默认 1;非 SVIP 账号可实验 4/8
        #[arg(long, default_value_t = 1)]
        parallel: u64,
        /// 块缓存总上限 MB(FIFO 淘汰)
        #[arg(long, default_value_t = 128)]
        cache_mb: u64,
        /// 允许其他用户访问挂载点(需要 /etc/fuse.conf 开 user_allow_other)
        #[arg(long)]
        allow_other: bool,
        /// 只读挂载(默认读写:写改动在 close 时按 4MB 分片三段式上传)
        #[arg(long)]
        read_only: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    // 裸跑:交互控制台(新机器按菜单走完登录/挂载/自启)
    let cmd = match cli.cmd {
        None => return menu::run(),
        Some(c) => c,
    };    match cmd {
        Cmd::Login {
            app_key,
            app_secret,
            code_mode,
        } => {
            baidu::AppConfig {
                app_key: app_key.clone(),
                app_secret: app_secret.clone(),
            }
            .save()
            .ok();
            if code_mode {
                baidu::authcode_login(&app_key, &app_secret)?;
            } else {
                baidu::device_login(&app_key, &app_secret)?;
            }
        }
        Cmd::Info => {
            let mut c = baidu::BaiduClient::from_config()?;
            let u = c.uinfo()?;
            println!("百度账号: {}", u.baidu_name);
            println!("网盘账号: {}", u.netdisk_name);
            println!("用户 ID: {}", u.uk);
            println!(
                "会员类型: {}",
                match u.vip_type {
                    2 => "超级会员",
                    1 => "会员",
                    _ => "普通",
                }
            );
            let (total, used) = c.quota()?;
            println!(
                "容量: 总 {total:.2} GB,已用 {used:.2} GB",
                total = total as f64 / 1e9,
                used = used as f64 / 1e9
            );
        }
        Cmd::Ls { path } => {
            let mut c = baidu::BaiduClient::from_config()?;
            for f in c.list_dir(&path)? {
                println!(
                    "{}\t{}\t{}",
                    if f.is_dir { "d" } else { "-" },
                    humansize(f.size),
                    f.name
                );
            }
        }
        #[cfg(windows)]
        Cmd::Sync { sync_root } => {
            let mut st = settings::Settings::load();
            if let Some(sr) = sync_root {
                st.sync_root = sr;
            }
            win::run(&st)?;
        }
        #[cfg(windows)]
        Cmd::Tray { quiet } => win::tray::run(&settings::Settings::load(), quiet)?,
        #[cfg(windows)]
        Cmd::Config => win::config_gui::run()?,
        #[cfg(windows)]
        Cmd::Progress => {
            menu::progress_view();
            pause_before_exit();
        }
        #[cfg(windows)]
        Cmd::Unregister => win::unregister()?,
        #[cfg(unix)]
        Cmd::Mount {
            mountpoint,
            root,
            dir_ttl,
            dlink_ttl,
            block_mb,
            parallel,
            cache_mb,
            allow_other,
            read_only,
        } => {
            std::fs::create_dir_all(&mountpoint)?;
            let client = baidu::BaiduClient::from_config()?;
            let panfs = fs::PanFs::new(
                client,
                &root,
                dir_ttl,
                dlink_ttl,
                block_mb,
                parallel,
                cache_mb,
            );

            let mut opts = vec![
                MountOption::FSName("bdfs".into()),
                MountOption::Subtype("baidupan".into()),
                // 默认读写;--read-only 让写操作在内核层就被拒绝(应用拿到 EROFS)
                // 注:内核 attr 缓存时长由我们在 reply.entry/attr 里返回的 TTL 决定
            ];
            // 挂载进程退出时自动卸载,避免留下访问不了的挂载点。
            // AutoUnmount 依赖 fusermount 二进制,没有时(如 Android)跳过,
            // 挂载走 root 直连 mount(2),退出由 Drop 里的 umount 收尾
            if fs::have_fusermount() {
                opts.push(MountOption::AutoUnmount);
            } else {
                println!("提示:未找到 fusermount,跳过 AutoUnmount;进程被杀后挂载点若残留,umount 清理");
            }
            if read_only {
                opts.push(MountOption::RO);
            }
            if allow_other {
                opts.push(MountOption::AllowOther);
            }

            println!(
                "挂载 {} -> {:?}({}),目录缓存 {dir_ttl}s,块 {block_mb}MB×{parallel} 并发,缓存上限 {cache_mb}MB",
                root,
                mountpoint,
                if read_only { "只读" } else { "读写:写改动在 close 时上传" },
            );
            println!("Ctrl-C 结束进程;若挂载点残留,用 fusermount -u 卸载");
            // 先留进度记录器的引用,卸载后清掉进度文件
            let prog = panfs.progress().clone();
            fuser::mount2(panfs, &mountpoint, &opts)?;
            prog.clear();
            println!("已卸载");
        }
    }
    Ok(())
}

/// 简单的人读字节数(避免引额外 crate)
fn humansize(n: u64) -> String {
    let unit = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < unit.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", unit[i])
    } else {
        format!("{v:.1} {}", unit[i])
    }
}

/// 托盘「传输进度…」拉起的新控制台窗口里跑完别闪退:等一个回车。
/// 刻意 cfg(windows) 只服务托盘;unix 下加无条件暂停会毁脚本管道
#[cfg(windows)]
fn pause_before_exit() {
    use std::io::BufRead;
    println!("按回车退出…");
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

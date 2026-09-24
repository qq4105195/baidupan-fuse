//! bdfs:多网盘 FUSE 挂载工具(Linux/macOS),支持读 + 写(close 时上传)。
//! 后端:百度网盘(开放平台 API)、联通云盘(wopan)。
//! 裸跑(不带子命令)进交互控制台,子命令供脚本使用;
//! --backend 选网盘(默认用设置里当前选中的,再默认百度)。

mod daemon;
mod fs;
mod menu;
mod pan;
mod progress;
mod settings;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use fuser::MountOption;
use pan::PanKind;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "bdfs",
    version,
    about = "多网盘 FUSE 挂载(百度网盘/联通云盘 + fuser);不带子命令进入交互控制台"
)]
struct Cli {
    /// 网盘后端:baidu(百度网盘)/ wopan(联通云盘)。
    /// 缺省用设置里当前选中的网盘(「1. 切换网盘」改),再默认百度
    #[arg(long, global = true, value_name = "baidu|wopan")]
    backend: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 登录。百度:--app-key/--app-secret(设备码或授权码);
    /// 联通:--phone 短信验证码(或 --token 手动粘抓包 token)
    Login {
        /// 百度:开放平台应用的 AppKey(client_id)
        #[arg(short = 'k', long)]
        app_key: Option<String>,
        /// 百度:开放平台应用的 SecretKey(client_secret)
        #[arg(short = 's', long)]
        app_secret: Option<String>,
        /// 百度:跳过设备码,直接用授权码模式(应用没开通设备码授权时用)
        #[arg(long)]
        code_mode: bool,
        /// 联通:手机号,发送短信验证码后在终端输入验证码
        #[arg(short, long)]
        phone: Option<String>,
        /// 联通:跳过短信,直接用 pan.wo.cn 抓包的 accessToken
        #[arg(short, long)]
        token: Option<String>,
        /// 联通:配合 --token,refreshToken(有才能自动续期)
        #[arg(long)]
        refresh_token: Option<String>,
    },
    /// 显示账号和网盘容量(验证登录是否有效)
    Info,
    /// 列远端目录(不挂载,冒烟测试用)
    Ls {
        /// 远端目录,默认根目录
        path: String,
    },
    /// 挂载到本地目录(Ctrl-C 退出后用 fusermount -u 卸载)。
    /// 所有参数都可省:省略的从 settings.json 里该网盘的设置读
    /// (「8. 设置」改的就是它),再缺就用后端默认值——
    /// systemd 模板服务 bdfs@.service 就靠这个做到"参数不烤进服务"
    Mount {
        /// 本地挂载点(缺省用设置里的,百度默认 /mnt/pan、联通 /mnt/wopan)
        mountpoint: Option<PathBuf>,
        /// 挂载的远端根目录(缺省用设置里的,默认 /;百度未过审应用只能访问 /apps/<应用名>)
        #[arg(short = 'r', long)]
        root: Option<String>,
        /// 目录列表缓存秒数(缺省用设置里的,默认 60,调大省 API 配额)
        #[arg(long)]
        dir_ttl: Option<u64>,
        /// 下载直链缓存秒数(缺省用设置里的;百度默认 1800、联通 600)
        #[arg(long)]
        dlink_ttl: Option<u64>,
        /// 顺序读块大小 MB:按块拉取+缓存,内核 128KB 小读全部命中缓存。
        /// 实测块越大吞吐越高(缺省用设置里的,默认 16)
        #[arg(long)]
        block_mb: Option<u64>,
        /// 每块并发连接数。实测百度 SVIP 账号单长流才是高速通道,
        /// 并发波浪会被 CDN 限速(缺省用设置里的,默认 1)
        #[arg(long)]
        parallel: Option<u64>,
        /// 块缓存总上限 MB(FIFO 淘汰;缺省用设置里的,默认 128)
        #[arg(long)]
        cache_mb: Option<u64>,
        /// 允许其他用户访问挂载点(需要 /etc/fuse.conf 开 user_allow_other)
        #[arg(long)]
        allow_other: bool,
        /// 只读挂载(默认读写:写改动在 close 时上传)
        #[arg(long)]
        read_only: bool,
        /// 后台挂载:fork 出守护进程,挂上就返回(日志在 config 目录 log-<网盘>.txt)
        #[arg(long)]
        daemon: bool,
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
    // --backend 显式给出就用它;否则用设置里当前选中的网盘(老用户=百度)
    let kind = match &cli.backend {
        Some(s) => PanKind::parse(s)
            .ok_or_else(|| anyhow::anyhow!("不认识的网盘 {s:?}:用 baidu 或 wopan"))?,
        None => settings::Config::load().current_kind(),
    };

    // 裸跑:交互控制台(新机器按菜单走完登录/挂载/自启)
    let cmd = match cli.cmd {
        None => return menu::run(),
        Some(c) => c,
    };
    match cmd {
        Cmd::Login {
            app_key,
            app_secret,
            code_mode,
            phone,
            token,
            refresh_token,
        } => match kind {
            PanKind::Baidu => {
                let (Some(ak), Some(sk)) = (app_key, app_secret) else {
                    bail!("百度登录需要 --app-key 和 --app-secret(开放平台应用凭据)");
                };
                pan::baidu::AppConfig {
                    app_key: ak.clone(),
                    app_secret: sk.clone(),
                }
                .save()
                .ok();
                if code_mode {
                    pan::baidu::authcode_login(&ak, &sk)?;
                } else {
                    pan::baidu::device_login(&ak, &sk)?;
                }
            }
            PanKind::Wopan => {
                if let Some(at) = token {
                    pan::wopan::token_login(&at, refresh_token.as_deref().unwrap_or(""))?;
                } else if let Some(phone) = phone {
                    pan::wopan::send_sms_code(&phone)?;
                    println!("验证码已发送到 {phone},输入短信验证码:");
                    let mut code = String::new();
                    std::io::stdin().read_line(&mut code)?;
                    pan::wopan::sms_login(&phone, code.trim())?;
                } else {
                    bail!("联通登录需要 --phone <手机号>(短信验证码)或 --token <accessToken>");
                }
            }
        },
        Cmd::Info => {
            let mut c = pan::client_for(kind)?;
            println!("{}", c.account()?);
            let (total, used) = c.quota()?;
            println!(
                "容量: 总 {total:.2} GB,已用 {used:.2} GB",
                total = total as f64 / 1e9,
                used = used as f64 / 1e9
            );
        }
        Cmd::Ls { path } => {
            let mut c = pan::client_for(kind)?;
            for f in c.list_dir(&path)? {
                println!(
                    "{}\t{}\t{}",
                    if f.is_dir { "d" } else { "-" },
                    humansize(f.size),
                    f.name
                );
            }
        }
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
            daemon,
        } => {
            // 显式参数 > settings.json 该网盘的设置 > 后端默认值。
            // 布尔开关只能"强制开":设置里开了只读,命令行不加 --read-only 也保持只读
            let st = settings::Config::load().settings_for(kind).clone();
            let mountpoint = mountpoint.unwrap_or_else(|| PathBuf::from(&st.mountpoint));
            let root = root.unwrap_or_else(|| st.root.clone());
            let dir_ttl = dir_ttl.unwrap_or(st.dir_ttl);
            let dlink_ttl = dlink_ttl.unwrap_or(st.dlink_ttl);
            let block_mb = block_mb.unwrap_or(st.block_mb);
            let parallel = parallel.unwrap_or(st.parallel);
            let cache_mb = cache_mb.unwrap_or(st.cache_mb);
            let read_only = read_only || st.readonly;
            let allow_other = allow_other || st.allow_other;
            // 已挂载就拒绝:叠层会把旧挂载盖住(旧进程一死就成了打不开的僵尸层)
            if fs::is_mounted(&mountpoint.to_string_lossy()) {
                bail!(
                    "{} 已在挂载中,别叠层;先卸载:fusermount -u {}",
                    mountpoint.display(),
                    mountpoint.display()
                );
            }
            std::fs::create_dir_all(&mountpoint)?;
            // sudo 挂载的坑:没开 allow_other 时,fuser 因 AutoUnmount 会自动加
            // allow_other,但会话 ACL 仍是"仅挂载者"——root 挂的盘其他用户
            // 一律 EACCES。提前说清楚,别等用户 ls 报"权限不够"才排查
            if unsafe { libc::geteuid() == 0 } && !allow_other {
                println!(
                    "⚠ 以 root 挂载:默认只有 root 能访问挂载点(会话 ACL)。\n\
                     其他用户要用,请去掉 sudo 以普通用户挂载,或在设置里开 allow_other。"
                );
            }

            let mut opts = vec![
                MountOption::FSName("bdfs".into()),
                MountOption::Subtype(
                    match kind {
                        PanKind::Baidu => "baidupan",
                        PanKind::Wopan => "wopanfs",
                    }
                    .into(),
                ),
                // 默认读写;--read-only 让写操作在内核层就被拒绝(应用拿到 EROFS)
                // 挂载进程退出时自动卸载,避免留下访问不了的挂载点
                MountOption::AutoUnmount,
                // 注:内核 attr 缓存时长由我们在 reply.entry/attr 里返回的 TTL 驱动
            ];
            if read_only {
                opts.push(MountOption::RO);
            }
            if allow_other {
                // allow_other 必须配 default_permissions:权限交给内核按属主+mode
                // 检查;不配的话内核把裁决推给服务端 access 回调,不实现等于放行所有人
                opts.push(MountOption::AllowOther);
                opts.push(MountOption::DefaultPermissions);
            }

            println!(
                "挂载 {}({}) -> {:?}({}),目录缓存 {dir_ttl}s,块 {block_mb}MB×{parallel} 并发,缓存上限 {cache_mb}MB",
                root,
                kind.label(),
                mountpoint,
                if read_only { "只读" } else { "读写:写改动在 close 时上传" },
            );
            if daemon {
                // 客户端/PanFs 在闭包里(fork 后)构造:reqwest::blocking 的内部
                // 运行时线程不跨 fork,父进程建好的到子进程里全是超时
                daemon::spawn(kind, &mountpoint.to_string_lossy(), opts, move || {
                    let client = pan::client_for(kind)?;
                    Ok(fs::PanFs::new(
                        client, &root, dir_ttl, dlink_ttl, block_mb, parallel, cache_mb,
                    ))
                })?;
            } else {
                let client = pan::client_for(kind)?;
                let panfs = fs::PanFs::new(
                    client, &root, dir_ttl, dlink_ttl, block_mb, parallel, cache_mb,
                );
                println!("Ctrl-C 结束进程;若挂载点残留,用 fusermount -u 卸载");
                // 先留进度记录器的引用,卸载后清掉进度文件
                let prog = panfs.progress().clone();
                fuser::mount2(panfs, &mountpoint, &opts)?;
                prog.clear();
                println!("已卸载");
            }
        }
    }
    Ok(())
}

/// 简单的人读字节数(避免引额外 crate)
pub fn humansize(n: u64) -> String {
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

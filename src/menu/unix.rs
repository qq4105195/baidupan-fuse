//! 控制台 Unix 菜单项:3 挂载 / 4 卸载 / 5 systemd 自启 / 6 取消自启 / 7 设置。
//! 内容自原 menu.rs 平移,行为不变。

use super::{ask_default, ask_num};
use crate::baidu;
use crate::fs::PanFs;
use crate::settings::Settings;
use anyhow::Result;
use fuser::MountOption;
use std::ffi::CString;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// systemd 服务名(unit 文件名去掉 .service)
const UNIT: &str = "bdfs";

pub fn status(st: &Settings) -> String {
    if is_mounted(&st.mountpoint) {
        format!("挂载:已挂载({})", st.mountpoint)
    } else {
        "挂载:未挂载".into()
    }
}

pub fn autostart_status() -> String {
    unit_state()
}

pub fn print_items() {
    println!("  3. 挂载        前台运行,Ctrl-C 退出并自动卸载");
    println!("  4. 卸载");
    println!("  5. 开机自动挂载(安装 systemd 服务)");
    println!("  6. 取消自动挂载");
    println!("  7. 设置        挂载点/根目录/块大小/并发/缓存/只读");
}

pub fn dispatch(choice: &str, st: &Settings) {
    match choice {
        "3" => {
            if let Err(e) = mount(st) {
                println!("挂载失败:{e:#}");
            }
        }
        "4" => unmount(st),
        "5" => install_unit(st),
        "6" => remove_unit(),
        "7" => configure(),
        _ => unreachable!("菜单 3-7 由共享骨架过滤后才进来"),
    }
}

/// 挂载:按设置前台挂载(mount2 阻塞到卸载;要常驻用「5. 开机自动挂载」)
fn mount(st: &Settings) -> Result<()> {
    let client = baidu::BaiduClient::from_config()?;
    std::fs::create_dir_all(&st.mountpoint)?;
    let panfs = PanFs::new(
        client,
        &st.root,
        st.dir_ttl,
        st.dlink_ttl,
        st.block_mb,
        st.parallel,
        st.cache_mb,
    );
    let mut opts = vec![
        MountOption::FSName("bdfs".into()),
        MountOption::Subtype("baidupan".into()),
        // 默认读写:写改动 close 时三段式上传;设置里开了只读才挂 RO
    ];
    // 挂载进程退出时自动卸载;AutoUnmount 依赖 fusermount,没有的环境(如 Android)跳过
    if crate::fs::have_fusermount() {
        opts.push(MountOption::AutoUnmount);
    }
    if st.readonly {
        opts.push(MountOption::RO);
    }
    if st.allow_other {
        opts.push(MountOption::AllowOther);
    }
    println!(
        "挂载 {} -> {}({},块 {}MB×{} 连接,缓存 {}MB)",
        st.root,
        st.mountpoint,
        if st.readonly { "只读" } else { "读写" },
        st.block_mb,
        st.parallel,
        st.cache_mb
    );
    println!("前台运行:Ctrl-C 退出并自动卸载;要后台常驻,先「7. 设置」调好,再「5. 开机自动挂载」。");
    let prog = panfs.progress().clone();
    fuser::mount2(panfs, &st.mountpoint, &opts)?;
    prog.clear();
    println!("已卸载。");
    Ok(())
}

/// 卸载:先结束挂载进程(auto_unmount 让内核自动摘挂载),兜底再显式 umount
fn unmount(st: &Settings) {
    let me = std::process::id() as i32;
    let pids = mount_pids(me);
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    if !pids.is_empty() {
        std::thread::sleep(Duration::from_millis(500));
    }
    if is_mounted(&st.mountpoint) {
        // 进程没了还挂着(异常退出残留),显式卸载;root 下可行
        match CString::new(st.mountpoint.clone()) {
            Ok(c) => {
                if unsafe { libc::umount2(c.as_ptr(), 0) } != 0 {
                    println!(
                        "umount {} 失败:{}(试试 fusermount -u {})",
                        st.mountpoint,
                        io::Error::last_os_error(),
                        st.mountpoint
                    );
                    return;
                }
            }
            Err(_) => {
                println!("挂载点路径含非法字符,无法卸载");
                return;
            }
        }
    } else if pids.is_empty() {
        println!("没有在跑的挂载。");
        return;
    }
    println!("已卸载 {}。", st.mountpoint);
}

/// 装 systemd 服务:ExecStart 用当前二进制 + 当前设置拼出来,
/// 之后改了设置要重跑一次「5」才会更新到服务里
fn install_unit(st: &Settings) {
    if !is_root() {
        println!("写 /etc/systemd/system 需要 root:请 sudo 运行 bdfs 再选 5。");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        println!("拿不到自己的可执行文件路径");
        return;
    };
    let mut cmd = format!("{:?} mount {:?} -r {:?}", exe, st.mountpoint, st.root);
    cmd.push_str(&format!(
        " --block-mb {} --parallel {} --cache-mb {} --dir-ttl {} --dlink-ttl {}",
        st.block_mb, st.parallel, st.cache_mb, st.dir_ttl, st.dlink_ttl
    ));
    if st.allow_other {
        cmd.push_str(" --allow-other");
    }
    if st.readonly {
        cmd.push_str(" --read-only");
    }
    let unit = format!(
        "[Unit]\nDescription=百度网盘 FUSE 挂载({})\nAfter=network-online.target\n\n\
         [Service]\nExecStart={}\nEnvironment=RUST_LOG=info\nRestart=on-failure\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        st.mountpoint, cmd
    );
    let path = format!("/etc/systemd/system/{UNIT}.service");
    if let Err(e) = std::fs::write(&path, unit) {
        println!("写 {path} 失败:{e}");
        return;
    }
    println!("服务文件已写入 {path}");
    for (desc, args) in [
        ("daemon-reload", vec!["daemon-reload"]),
        ("enable --now", vec!["enable", "--now", UNIT]),
    ] {
        match Command::new("systemctl").args(&args).status() {
            Ok(s) if s.success() => {}
            r => {
                println!("systemctl {desc} 失败:{r:?}");
                return;
            }
        }
    }
    println!("开机自动挂载已启用,当前也已启动。systemctl status {UNIT} 查看状态。");
}

/// 取消自启:disable --now 会顺带停掉正在跑的服务(挂载一并卸载)
fn remove_unit() {
    let path = format!("/etc/systemd/system/{UNIT}.service");
    if !Path::new(&path).exists() {
        println!("没有安装过自启服务。");
        return;
    }
    if !is_root() {
        println!("需要 root:请 sudo 运行 bdfs 再选 6。");
        return;
    }
    match Command::new("systemctl")
        .args(["disable", "--now", UNIT])
        .status()
    {
        Ok(s) if s.success() => {}
        r => println!("systemctl disable 失败:{r:?}(继续删除 unit 文件)"),
    }
    let _ = std::fs::remove_file(&path);
    let _ = Command::new("systemctl").arg("daemon-reload").status();
    println!("已停止服务并取消开机自动挂载。");
}

/// 设置:逐项问,回车保持当前值
fn configure() {
    let mut st = Settings::load();
    println!("-- 设置(回车 = 保持当前值)--");
    let Some(mp) = ask_default("挂载点", Some(&st.mountpoint)) else {
        return;
    };
    st.mountpoint = mp;
    let Some(root) = ask_default("远端根目录(挂全盘填 /)", Some(&st.root)) else {
        return;
    };
    st.root = root;
    let Some(bm) = ask_num("块大小 MB(越大吞吐越高,16 是甜点)", st.block_mb) else {
        return;
    };
    st.block_mb = bm;
    let Some(p) = ask_num("并发连接数(SVIP 账号保持 1,并发会被限速)", st.parallel) else {
        return;
    };
    st.parallel = p;
    let Some(cm) = ask_num("块缓存上限 MB", st.cache_mb) else {
        return;
    };
    st.cache_mb = cm;
    let Some(ao) = ask_default("允许其他用户访问挂载点?y/N", None) else {
        return;
    };
    st.allow_other = matches!(ao.as_str(), "y" | "Y" | "yes" | "是");
    let Some(ro) = ask_default("只读挂载?(默认读写,写改动 close 时上传)y/N", None) else {
        return;
    };
    st.readonly = matches!(ro.as_str(), "y" | "Y" | "yes" | "是");
    if let Err(e) = st.save() {
        println!("保存失败:{e:#}");
    }
}

fn unit_state() -> String {
    if Path::new(&format!("/etc/systemd/system/{UNIT}.service")).exists() {
        "已启用".into()
    } else {
        "未配置".into()
    }
}

/// 查 /proc/mounts 第二列判断挂载点是否挂着
fn is_mounted(mp: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|s| {
            s.lines().any(|l| {
                let mut it = l.split(' ');
                it.next();
                matches!(it.next(), Some(m) if m == mp)
            })
        })
        .unwrap_or(false)
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// 找出所有挂载进程(命令行是 bdfs/baidupan-fuse 且带 mount 子命令),排除自己。
/// 自己扫 /proc 按 pid 杀,比 `pkill -f` 安全——后者会误杀命令行里恰好
/// 含同样字符串的进程(比如正在跑这条命令的 ssh 会话本身)
fn mount_pids(me: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let parts: Vec<String> = raw
            .split(|b: &u8| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let exe = Path::new(&parts[0])
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&parts[0]);
        if (exe == "bdfs" || exe == "baidupan-fuse") && parts[1..].iter().any(|a| a == "mount") {
            out.push(pid);
        }
    }
    out
}

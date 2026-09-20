//! 控制台 Windows 菜单项:3 启动按需同步 / 4 停止 / 5 开机自启(注册表 Run 键)/
//! 6 取消自启 / 7 设置。同步本体在 crate::win(Cloud Files API 提供方)。

use super::{ask_default, ask_num};
use crate::settings::Settings;

/// HKCU Run 键里自启项的名字
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "bdfs";

pub fn status(st: &Settings) -> String {
    if crate::win::is_registered(&st.sync_root) {
        format!("同步:已注册({})", st.sync_root)
    } else {
        format!("同步:未注册(根:{})", st.sync_root)
    }
}

pub fn autostart_status() -> String {
    if autostart_exists() {
        "已启用".into()
    } else {
        "未配置".into()
    }
}

pub fn print_items() {
    println!("  3. 同步        启动按需文件夹,前台运行,Ctrl-C 退出");
    println!("  4. 停止同步");
    println!("  5. 开机自动同步(登录启动托盘,托盘自动带起同步)");
    println!("  6. 取消自动同步");
    println!("  7. 设置        同步根/远端根目录/缓存");
}

pub fn dispatch(choice: &str, st: &Settings) {
    match choice {
        "3" => {
            if let Err(e) = crate::win::run(st) {
                println!("同步失败:{e:#}");
            }
        }
        "4" => stop(st),
        "5" => install_autostart(),
        "6" => remove_autostart(),
        "7" => configure(),
        _ => unreachable!("菜单 3-7 由共享骨架过滤后才进来"),
    }
}

/// 停止同步:命名事件通知正在跑的同步进程(它走正常断开流程,不注销)
fn stop(st: &Settings) {
    if crate::win::stop(&st.sync_root) {
        println!("已通知同步进程退出。");
    } else {
        println!("没有在跑的同步进程。");
    }
}

fn install_autostart() {
    match autostart_enable() {
        Ok(cmd) => println!("开机自动同步已启用(登录后启动托盘:{cmd})。"),
        Err(e) => println!("启用失败:{e:#}"),
    }
}

fn remove_autostart() {
    if !autostart_exists() {
        println!("没有配置过自启。");
        return;
    }
    match autostart_disable() {
        Ok(()) => println!("已取消开机自动同步。"),
        Err(e) => println!("取消失败:{e:#}"),
    }
}

/// 写 Run 键:登录后启动托盘(托盘自动带起同步)。纯注册表操作无打印——
/// 托盘进程脱离控制台后也要能调。返回写入的命令串供控制台回显
pub(crate) fn autostart_enable() -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let cmd = format!("\"{}\" tray", exe.display());
    let ok = std::process::Command::new("reg")
        .args(["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &cmd, "/f"])
        .status()
        .map_err(|e| anyhow::anyhow!("启动 reg 失败:{e}"))?
        .success();
    if ok {
        Ok(cmd)
    } else {
        anyhow::bail!("reg add 失败(退出码非 0)")
    }
}

/// 删 Run 键(纯操作无打印,理由同上)
pub(crate) fn autostart_disable() -> anyhow::Result<()> {
    let ok = std::process::Command::new("reg")
        .args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"])
        .status()
        .map_err(|e| anyhow::anyhow!("启动 reg 失败:{e}"))?
        .success();
    if ok {
        Ok(())
    } else {
        anyhow::bail!("reg delete 失败(退出码非 0)")
    }
}

pub(crate) fn autostart_exists() -> bool {
    // 值不存在时 reg query 退出码非 0(本机实测过),status 判定足够
    std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 设置:Windows 只问同步相关项(块大小/缓存等是 FUSE 概念)
pub(crate) fn configure() {
    let mut st = Settings::load();
    println!("-- 设置(回车 = 保持当前值)--");
    let Some(sr) = ask_default("同步根目录(改了要重新注册,原根先停止同步)", Some(&st.sync_root))
    else {
        return;
    };
    st.sync_root = sr;
    let Some(root) = ask_default(
        "远端根目录(未过审应用只能访问 /apps/<应用名>,挂全盘填 /)",
        Some(&st.root),
    ) else {
        return;
    };
    st.root = root;
    let Some(dt) = ask_num(
        "目录列表缓存秒数(省 API 配额;未过审应用 10 次/小时,建议 300)",
        st.dir_ttl,
    ) else {
        return;
    };
    st.dir_ttl = dt;
    let Some(dlt) = ask_num("下载直链缓存秒数(官方 8 小时有效)", st.dlink_ttl) else {
        return;
    };
    st.dlink_ttl = dlt;
    if let Err(e) = st.save() {
        println!("保存失败:{e:#}");
    }
}

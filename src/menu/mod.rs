//! bdfs 交互控制台(平台共享骨架):不带子命令直接运行时进入。
//! 定位是"新机器上手"——按菜单提示完成登录、挂载/同步、开机自启;
//! 脚本化/自动化场景请继续用子命令(bdfs login / mount / sync / info / ls)。
//! 菜单 3~7 按平台分派:unix.rs(FUSE 挂载 + systemd)/ windows.rs(按需同步 + 注册表自启)。

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use crate::baidu;
use crate::settings::Settings;
use anyhow::Result;
use std::io::{self, Write};

pub fn run() -> Result<()> {
    println!("bdfs 控制台(脚本化请用子命令:bdfs login / mount / sync / info / ls …)");
    loop {
        let st = Settings::load();
        println!();
        println!("════════════ bdfs ════════════");
        println!(
            "凭据:{}  {}  自启:{}",
            cred_state(),
            platform::status(&st),
            platform::autostart_status()
        );
        println!("──────────────────────────────");
        println!("  1. 登录授权    填 AppKey/SecretKey,浏览器拿授权码");
        println!("  2. 账号信息    验证登录、看容量");
        platform::print_items();
        println!("  8. 传输进度    挂载/同步进程正在/最近的下载与上传");
        println!("  0. 退出");
        let Some(choice) = ask("选择") else { break };
        match choice.as_str() {
            "1" => login(),
            "2" => info(),
            "8" => progress_view(),
            "0" => break,
            c @ ("3" | "4" | "5" | "6" | "7") => platform::dispatch(c, &st),
            _ => println!("没有这个选项,输数字 0-8。"),
        }
    }
    Ok(())
}

// ---------- 共享菜单项 ----------

/// 登录:先问应用凭据(回车沿用已保存的),再走 oob 授权码流程
fn login() {
    println!("-- 登录授权 --");
    let saved = baidu::AppConfig::load().ok();
    let Some(ak) = ask_default("AppKey", saved.as_ref().map(|c| c.app_key.as_str())) else {
        return;
    };
    if ak.is_empty() {
        println!("AppKey 不能为空。");
        return;
    }
    let Some(sk) = ask_default("AppSecret", saved.as_ref().map(|c| c.app_secret.as_str())) else {
        return;
    };
    if sk.is_empty() {
        println!("AppSecret 不能为空。");
        return;
    }
    if let Err(e) = (
        baidu::AppConfig {
            app_key: ak.clone(),
            app_secret: sk.clone(),
        }
        .save()
    ) {
        println!("保存应用凭据失败:{e:#}");
        return;
    }
    println!("应用凭据已保存。开始授权:浏览器打开网址 → 同意 → 把页面上的授权码粘回来");
    match baidu::authcode_login(&ak, &sk) {
        Ok(_) => println!("\n登录完成。可用「2. 账号信息」验证。"),
        Err(e) => println!("登录失败:{e:#}"),
    }
}

/// 账号信息:uinfo + quota
fn info() {
    let mut c = match baidu::BaiduClient::from_config() {
        Ok(c) => c,
        Err(e) => {
            println!("凭据不可用({e:#}),先选「1. 登录授权」。");
            return;
        }
    };
    let u = match c.uinfo() {
        Ok(u) => u,
        Err(e) => {
            println!("查询失败:{e:#}(token 可能已失效,重新登录试试)");
            return;
        }
    };
    println!("百度账号:{}  网盘账号:{}", u.baidu_name, u.netdisk_name);
    println!(
        "会员类型:{}",
        match u.vip_type {
            2 => "超级会员",
            1 => "会员",
            _ => "普通",
        }
    );
    match c.quota() {
        Ok((total, used)) => println!(
            "容量:总 {:.2} GB,已用 {:.2} GB",
            total as f64 / 1e9,
            used as f64 / 1e9
        ),
        Err(e) => println!("容量查询失败:{e:#}"),
    }
}

// ---------- 小工具(平台子模块共用) ----------

/// 下载进度:读挂载/同步进程写的 progress.json
fn progress_view() {
    use crate::progress::ProgressFile;
    let p = baidu::config_dir().join("progress.json");
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(_) => {
            println!("没有进度记录(挂载/同步没在跑,或还没读过文件)。");
            return;
        }
    };
    let v: ProgressFile = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            println!("进度文件解析失败:{e}");
            return;
        }
    };
    if v.running.is_empty() && v.recent.is_empty() {
        println!("还没有下载活动。");
        return;
    }
    let now = crate::progress::now_ms();
    // 进度停更多半是挂载进程没了/卡住,提示而不是干等
    let stale = now.saturating_sub(v.updated_ms);
    if stale > 5000 {
        println!("(注意:进度已 {} 秒没更新,进程可能没在跑)", stale / 1000);
    }
    for e in &v.running {
        let pct = if e.total > 0 {
            e.done as f64 / e.total as f64 * 100.0
        } else {
            0.0
        };
        let secs = now.saturating_sub(e.started_ms).max(1) as f64 / 1000.0;
        let speed = e.done as f64 / secs / 1e6;
        println!(
            "[进行] {} {} @{} {}/{} ({:.0}%) {:.1}MB/s",
            e.path,
            e.kind,
            crate::humansize(e.offset),
            crate::humansize(e.done),
            crate::humansize(e.total),
            pct,
            speed
        );
    }
    for d in v.recent.iter().take(10) {
        let speed = d.bytes as f64 / d.ms.max(1) as f64 / 1e3;
        println!(
            "[{}] {} {} {} 耗时 {:.1}s({:.1}MB/s)",
            if d.ok { "完成" } else { "失败" },
            d.path,
            d.kind,
            crate::humansize(d.bytes),
            d.ms as f64 / 1000.0,
            speed
        );
    }
}

/// 读一行输入(去空白),None = EOF(Ctrl-D)。默认值非空时显示 [默认]
fn ask_default(prompt: &str, default: Option<&str>) -> Option<String> {
    let def = default.filter(|d| !d.is_empty());
    match def {
        Some(d) => print!("{prompt} [{d}]: "),
        None => print!("{prompt}: "),
    }
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let t = line.trim().to_string();
    Some(if t.is_empty() {
        def.unwrap_or("").to_string()
    } else {
        t
    })
}

fn ask(prompt: &str) -> Option<String> {
    ask_default(prompt, None)
}

/// 读正整数,输错重问,回车保持默认
fn ask_num(prompt: &str, default: u64) -> Option<u64> {
    loop {
        let s = ask_default(prompt, Some(&default.to_string()))?;
        if s.is_empty() {
            return Some(default);
        }
        if let Ok(n) = s.parse::<u64>() {
            if n > 0 {
                return Some(n);
            }
        }
        println!("要一个正整数。");
    }
}

/// 凭据状态:config.json + token.json 都在就算已保存(不实际调 API)
fn cred_state() -> String {
    let dir = baidu::config_dir();
    if dir.join("token.json").is_file() && dir.join("config.json").is_file() {
        "已保存".into()
    } else {
        "未配置".into()
    }
}

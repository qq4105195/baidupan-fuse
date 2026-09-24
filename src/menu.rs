//! bdfs 交互控制台:不带子命令直接运行时进入。
//! 定位是"新机器上手"——按菜单提示完成登录、挂载、开机自启;
//! 脚本化/自动化场景请继续用子命令(bdfs --backend wopan login / mount …)。
//!
//! 多网盘:控制台永远操作"当前网盘"(设置里的 backend),
//! 「1. 切换网盘」换目标;各网盘的登录方式/凭据文件/挂载点/服务名互相独立。

use crate::pan::{self, PanKind};
use crate::settings::{Config, Settings};
use anyhow::Result;
use fuser::MountOption;
use std::ffi::CString;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// systemd 自启:一份模板 bdfs@.service,按网盘启实例 bdfs@baidu / bdfs@wopan。
/// ExecStart 只有 `--backend %i mount`,挂载参数不烤进服务——
/// 实例启动时自己从 settings.json 实时读,「8. 设置」改完
/// `systemctl restart bdfs@<网盘>` 就生效,不用重装服务。
/// 老版本是每个网盘一份独立服务(参数烤死在里面),检测到就迁移。
const TEMPLATE_PATH: &str = "/etc/systemd/system/bdfs@.service";

/// 模板单元名(带 @,无 .service)
fn unit_instance(kind: PanKind) -> String {
    format!("bdfs@{}", kind.id())
}

/// 老版本每个网盘的独立服务名(迁移用)
fn legacy_unit(kind: PanKind) -> &'static str {
    match kind {
        PanKind::Baidu => "bdfs",
        PanKind::Wopan => "bdfs-wopan",
    }
}

fn legacy_unit_installed(kind: PanKind) -> bool {
    Path::new(&format!("/etc/systemd/system/{}.service", legacy_unit(kind))).exists()
}

/// 模板内容(%i = 实例名即网盘 id)。RestartSec 稍等再拉,
/// 并保留 systemd 默认启动次数上限:凭据坏了别无限打 API(百度限频 10 次/小时)
fn template_body(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=bdfs FUSE 挂载(%i)\nAfter=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\nExecStart={:?} --backend %i mount\nEnvironment=RUST_LOG=info\n\
         Restart=on-failure\nRestartSec=2\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        exe
    )
}

/// 实例是否开了自启(读 systemd 状态,不需要 root)
fn instance_enabled(kind: PanKind) -> bool {
    Command::new("systemctl")
        .args(["is-enabled", &unit_instance(kind)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 状态行/管理页用的短名
fn short_label(kind: PanKind) -> &'static str {
    match kind {
        PanKind::Baidu => "百度",
        PanKind::Wopan => "联通",
    }
}

/// 状态行的自启概览:两个网盘一起显示(✓ 开 / ✗ 关 / 旧 = 老版服务待迁移)
fn autostart_summary() -> String {
    PanKind::all()
        .iter()
        .map(|k| {
            let mark = if instance_enabled(*k) {
                "✓"
            } else if legacy_unit_installed(*k) {
                "旧"
            } else {
                "✗"
            };
            format!("{}{}", short_label(*k), mark)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// /proc/mounts 里区分两个后端的 subtype
fn subtype(kind: PanKind) -> &'static str {
    match kind {
        PanKind::Baidu => "baidupan",
        PanKind::Wopan => "wopanfs",
    }
}

pub fn run() -> Result<()> {
    println!("bdfs 控制台(脚本化请用子命令:bdfs --backend <网盘> login / mount / info / ls …)");
    automount_at_startup();
    loop {
        let cfg = Config::load();
        let kind = cfg.current_kind();
        let st = cfg.settings_for(kind).clone();
        println!();
        println!("════════════ bdfs ════════════");
        println!(
            "当前网盘:{}  凭据:{}  挂载:{}  自启:{}",
            kind.label(),
            cred_state(kind),
            mount_state(&st.mountpoint),
            autostart_summary()
        );
        println!("──────────────────────────────");
        println!("  1. 切换网盘    当前:{},可在百度网盘/联通云盘之间切换", kind.label());
        println!("  2. 登录        {}", login_hint(kind));
        println!("  3. 账号信息    验证登录、看容量");
        println!("  4. 挂载        后台挂载,挂上就回来(启动控制台时也会自动挂载)");
        println!("  5. 卸载");
        println!("  6. 开机自动挂载 一处管理所有网盘的自启(不必切网盘)");
        println!("  7. 取消自动挂载 全部网盘关闭并清理服务");
        println!("  8. 设置        挂载点/根目录/块大小/并发/缓存/只读");
        println!("  9. 传输进度    挂载进程正在/最近的下载与上传");
        println!("  0. 退出");
        let Some(choice) = ask("选择") else { break };
        match choice.as_str() {
            "1" => switch_pan(),
            "2" => login(kind),
            "3" => info(kind),
            "4" => {
                if let Err(e) = mount(kind, &st) {
                    println!("挂载失败:{e:#}");
                }
            }
            "5" => unmount(kind, &st),
            "6" => autostart_menu(),
            "7" => disable_all_autostart(),
            "8" => configure(kind),
            "9" => progress_view(kind),
            "0" => break,
            _ => println!("没有这个选项,输数字 0-9。"),
        }
    }
    Ok(())
}

// ---------- 各菜单项 ----------

/// 切换网盘:列出所有后端选一个,写进设置
fn switch_pan() {
    let kinds = PanKind::all();
    for (i, k) in kinds.iter().enumerate() {
        println!("  {}. {}", i + 1, k.label());
    }
    let Some(s) = ask("选择网盘") else { return };
    let Some(k) = s.trim().parse::<usize>().ok().and_then(|i| kinds.get(i - 1)) else {
        println!("没有这个选项。");
        return;
    };
    let mut cfg = Config::load();
    cfg.backend = k.id().into();
    if let Err(e) = cfg.save() {
        println!("保存失败:{e:#}");
    }
    println!("已切换到 {}。", k.label());
}

fn login_hint(kind: PanKind) -> &'static str {
    match kind {
        PanKind::Baidu => "填 AppKey/SecretKey,浏览器拿授权码",
        PanKind::Wopan => "手机号 + 短信验证码(或手动粘贴 token)",
    }
}

/// 登录:按当前网盘走各自流程
fn login(kind: PanKind) {
    match kind {
        PanKind::Baidu => login_baidu(),
        PanKind::Wopan => login_wopan(),
    }
}

/// 百度:先问应用凭据(回车沿用已保存的),再走 oob 授权码流程
fn login_baidu() {
    println!("-- 百度网盘登录 --");
    let saved = pan::baidu::AppConfig::load().ok();
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
    if let Err(e) = (pan::baidu::AppConfig {
        app_key: ak.clone(),
        app_secret: sk.clone(),
    }
    .save())
    {
        println!("保存应用凭据失败:{e:#}");
        return;
    }
    println!("应用凭据已保存。开始授权:浏览器打开网址 → 同意 → 把页面上的授权码粘回来");
    match pan::baidu::authcode_login(&ak, &sk) {
        Ok(_) => println!("\n登录完成。可用「3. 账号信息」验证。"),
        Err(e) => println!("登录失败:{e:#}"),
    }
}

/// 联通:默认手机号+短信验证码;抓包党可以改用手动粘 token
fn login_wopan() {
    println!("-- 联通云盘登录 --");
    let Some(phone) = ask_default("手机号(输 t 改用手动粘贴 token)", None) else {
        return;
    };
    if phone.eq_ignore_ascii_case("t") {
        let Some(at) = ask("accessToken(pan.wo.cn 抓包)") else { return };
        if at.is_empty() {
            println!("accessToken 不能为空。");
            return;
        }
        let rt = ask("refreshToken(可空,没有就不能自动续期)").unwrap_or_default();
        if let Err(e) = pan::wopan::token_login(&at, &rt) {
            println!("保存失败:{e:#}");
        }
        return;
    }
    if let Err(e) = pan::wopan::send_sms_code(&phone) {
        println!("发送验证码失败:{e:#}");
        return;
    }
    println!("验证码已发送到 {phone}。");
    let Some(code) = ask("输入短信验证码") else { return };
    match pan::wopan::sms_login(&phone, &code) {
        Ok(()) => println!("登录完成。可用「3. 账号信息」验证。"),
        Err(e) => println!("登录失败:{e:#}(验证码错误可重试,注意别把两个 client 的密钥搞混)"),
    }
}

/// 账号信息:统一走 trait 的 account/quota
fn info(kind: PanKind) {
    let mut c = match pan::client_for(kind) {
        Ok(c) => c,
        Err(e) => {
            println!("凭据不可用({e:#}),先选「2. 登录」。");
            return;
        }
    };
    match c.account() {
        Ok(acc) => println!("{acc}"),
        Err(e) => {
            println!("查询失败:{e:#}(token 可能已失效,重新登录试试)");
            return;
        }
    }
    match c.quota() {
        Ok((total, used)) => println!(
            "容量:总 {:.2} GB,已用 {:.2} GB",
            total as f64 / 1e9,
            used as f64 / 1e9
        ),
        Err(e) => println!("容量查询失败:{e:#}"),
    }
}

/// 启动即挂载:有凭据、没挂载、非 root(root 进控制台一般是装自启服务,
/// 别顺手挂出 root 属主的层)就自动后台挂载当前网盘
fn automount_at_startup() {
    let cfg = Config::load();
    let kind = cfg.current_kind();
    let st = cfg.settings_for(kind).clone();
    if creds_saved(kind) && !crate::fs::is_mounted(&st.mountpoint) && !is_root() {
        println!("({} 已登录未挂载,自动后台挂载…)", kind.label());
        if let Err(e) = mount(kind, &st) {
            println!("自动挂载失败:{e:#}(菜单 4 可重试)");
        }
    }
}

/// 挂载:按当前网盘的设置后台挂载(daemon::spawn 挂上就返回)。
/// 要看实时日志/前台调试:bdfs --backend <网盘> mount
fn mount(kind: PanKind, st: &Settings) -> Result<()> {
    // 已挂载就拒绝:在挂载点上再挂一层会把旧层盖住(之前的进程死了就成僵尸层,
    // 访问全打在死层上),想重挂先「5. 卸载」
    if crate::fs::is_mounted(&st.mountpoint) {
        println!(
            "{} 已经挂着(菜单状态行也能看)。要重挂先选「5. 卸载」,别叠层。",
            st.mountpoint
        );
        return Ok(());
    }
    if st.allow_other && !is_root() && !fuse_conf_allows_user_allow_other() {
        println!(
            "提示:/etc/fuse.conf 未开 user_allow_other,普通用户挂载 allow_other 会被拒。\n\
             加一行 user_allow_other(sudo sh -c 'echo user_allow_other >> /etc/fuse.conf')后重试。"
        );
    }
    // 探路:凭据缺失/非法在这里就报,不用等子进程看日志
    pan::client_for(kind)?;
    std::fs::create_dir_all(&st.mountpoint)?;
    let mut opts = vec![
        MountOption::FSName("bdfs".into()),
        MountOption::Subtype(subtype(kind).into()),
        // 默认读写:写改动 close 时上传;设置里开了只读才挂 RO
        // 挂载进程退出时自动卸载
        MountOption::AutoUnmount,
    ];
    if st.readonly {
        opts.push(MountOption::RO);
    }
    if st.allow_other {
        // allow_other 必须配 default_permissions:权限交给内核按属主+mode 检查。
        // 不配的话内核把裁决推给服务端的 access 回调,不实现等于放行所有人
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::DefaultPermissions);
    }
    println!(
        "挂载 {}({}) -> {}({},块 {}MB×{} 连接,缓存 {}MB)",
        st.root,
        kind.label(),
        st.mountpoint,
        if st.readonly { "只读" } else { "读写" },
        st.block_mb,
        st.parallel,
        st.cache_mb
    );
    // 后台挂载:守护进程 fork 出去跑,这边等挂载点出现就返回菜单。
    // PanFs/客户端必须在闭包里(fork 之后)构造——reqwest::blocking 的内部
    // 运行时线程不跨 fork,父进程建好的到子进程里全是超时
    crate::daemon::spawn(kind, &st.mountpoint, opts, move || {
        let client = pan::client_for(kind)?;
        Ok(crate::fs::PanFs::new(
            client,
            &st.root,
            st.dir_ttl,
            st.dlink_ttl,
            st.block_mb,
            st.parallel,
            st.cache_mb,
        ))
    })?;
    Ok(())
}

/// 卸载:先结束挂载进程(auto_unmount 让内核自动摘挂载),兜底再显式 umount。
/// 挂载点上可能叠了好几层(历史 bug/手动 mount 过),逐层剥干净
fn unmount(kind: PanKind, st: &Settings) {
    let me = std::process::id() as i32;
    let pids = mount_pids(kind, me);
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    if !pids.is_empty() {
        std::thread::sleep(Duration::from_millis(500));
    }
    if pids.is_empty() && !crate::fs::is_mounted(&st.mountpoint) {
        println!("没有在跑的挂载。");
        return;
    }
    // 逐层 umount,剥到不挂为止(root 下的挂载层普通用户剥不动,提示 sudo)
    let mut peeled = 0u32;
    while crate::fs::is_mounted(&st.mountpoint) && peeled < 8 {
        match CString::new(st.mountpoint.clone()) {
            Ok(c) => {
                if unsafe { libc::umount2(c.as_ptr(), 0) } != 0 {
                    if peeled == 0 {
                        println!(
                            "umount {} 失败:{}(root 挂的层要 sudo:fusermount -u {})",
                            st.mountpoint,
                            io::Error::last_os_error(),
                            st.mountpoint
                        );
                        return;
                    }
                    break;
                }
                peeled += 1;
            }
            Err(_) => {
                println!("挂载点路径含非法字符,无法卸载");
                return;
            }
        }
    }
    if peeled > 1 {
        println!("(剥掉了叠着的 {peeled} 层挂载)");
    }
    println!("已卸载 {}。", st.mountpoint);
}

/// 「6. 开机自动挂载」管理页:所有网盘的自启一屏切换,不用先切网盘。
/// 选某个网盘 = 开/关切换;老版独立服务也在这里选择时自动迁移
fn autostart_menu() {
    println!("-- 开机自动挂载 --");
    println!("一份模板服务 + 按网盘启实例(bdfs@baidu / bdfs@wopan),实例启动时从设置实时读挂载参数。");
    loop {
        let kinds = PanKind::all();
        let cfg = Config::load();
        for (i, k) in kinds.iter().enumerate() {
            let st = cfg.settings_for(*k);
            let state = if instance_enabled(*k) {
                "已启用 → 选择=关闭"
            } else if legacy_unit_installed(*k) {
                "旧版服务 → 选择=迁移并启用"
            } else {
                "未启用 → 选择=开启"
            };
            println!("  {}. {:<5} 自启:{}  挂载点 {}", i + 1, short_label(*k), state, st.mountpoint);
        }
        println!("  {}. 全部开启", kinds.len() + 1);
        println!("  0. 返回");
        let Some(c) = ask("选择") else { return };
        match c.as_str() {
            "0" => return,
            "a" | "A" => {
                for k in kinds {
                    enable_autostart(k);
                }
            }
            _ => {
                let Ok(n) = c.parse::<usize>() else {
                    println!("没有这个选项。");
                    continue;
                };
                if n == kinds.len() + 1 {
                    for k in kinds {
                        enable_autostart(k);
                    }
                } else if let Some(k) = n.checked_sub(1).and_then(|i| kinds.get(i)) {
                    if instance_enabled(*k) || legacy_unit_installed(*k) {
                        disable_autostart(*k);
                    } else {
                        enable_autostart(*k);
                    }
                } else {
                    println!("没有这个选项。");
                }
            }
        }
    }
}

/// 「7. 取消自动挂载」:全部网盘关自启;都不启了顺手删掉模板文件
fn disable_all_autostart() {
    let targets: Vec<PanKind> = PanKind::all()
        .into_iter()
        .filter(|k| instance_enabled(*k) || legacy_unit_installed(*k))
        .collect();
    if targets.is_empty() {
        println!("没有网盘启用自启。");
        return;
    }
    for k in targets {
        disable_autostart(k);
    }
}

/// 给单个网盘开自启(需要 root):装/刷新模板,迁移老版服务,启用实例并立刻启动。
/// 实例以"安装者"身份跑(sudo 安装时写 drop-in 定住 User),不拿 root 挂载——
/// root 挂的 FUSE 文件全归 root,普通用户反而摸不了
fn enable_autostart(kind: PanKind) {
    if !is_root() {
        println!("写 {TEMPLATE_PATH} 需要 root:请 sudo 运行 bdfs 再操作。");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        println!("拿不到自己的可执行文件路径");
        return;
    };
    // 模板每次都重写:二进制挪了位置,路径也跟着更新
    if let Err(e) = std::fs::write(TEMPLATE_PATH, template_body(&exe)) {
        println!("写 {TEMPLATE_PATH} 失败:{e}");
        return;
    }
    migrate_legacy(kind);
    let inst = unit_instance(kind);
    let st = Config::load().settings_for(kind).clone();
    if let Some(u) = sudo_user() {
        let drop_dir = format!("/etc/systemd/system/{inst}.service.d");
        let drop = format!(
            "[Service]\n# 由 bdfs 安装时生成:以安装者身份挂载(配置/凭据/暂存/文件属性都归他)\nUser={}\nEnvironment=\"HOME={}\"\n",
            u.name, u.home
        );
        match std::fs::create_dir_all(&drop_dir)
            .and_then(|_| std::fs::write(format!("{drop_dir}/10-user.conf"), drop))
        {
            Ok(()) => {
                // fusermount 要求用户对挂载点可写,root 建的目录要交给他
                let _ = std::fs::create_dir_all(&st.mountpoint);
                if let Err(e) = std::os::unix::fs::chown(&st.mountpoint, Some(u.uid), Some(u.gid)) {
                    println!("chown {} 给 {} 失败:{e}(该用户可能挂不上)", st.mountpoint, u.name);
                }
            }
            Err(e) => println!("写 {drop_dir} 失败:{e}(实例将以 root 跑)"),
        }
    }
    // allow_other 需要 fuse.conf 放行,趁手里有 root 一起办了
    if st.allow_other {
        ensure_user_allow_other();
    }
    for (desc, args) in [
        ("daemon-reload", vec!["daemon-reload"]),
        ("enable --now", vec!["enable", "--now", inst.as_str()]),
    ] {
        match Command::new("systemctl").args(&args).status() {
            Ok(s) if s.success() => {}
            r => {
                println!("systemctl {desc} 失败:{r:?}");
                return;
            }
        }
    }
    println!(
        "{} 自启已启用,当前也已启动。systemctl status {inst} 看状态;\n改了「8. 设置」后 systemctl restart {inst} 生效。",
        kind.label()
    );
}

/// 关单个网盘的自启:disable --now 顺带停掉实例(挂载一并卸载)。
/// 两个网盘都不启了就把模板文件也删掉,机器不留死配置
fn disable_autostart(kind: PanKind) {
    if !instance_enabled(kind) && !legacy_unit_installed(kind) {
        println!("{} 没有启用自启。", kind.label());
        return;
    }
    if !is_root() {
        println!("需要 root:请 sudo 运行 bdfs 再操作。");
        return;
    }
    let inst = unit_instance(kind);
    if instance_enabled(kind) {
        match Command::new("systemctl")
            .args(["disable", "--now", inst.as_str()])
            .status()
        {
            Ok(s) if s.success() => {}
            r => println!("systemctl disable 失败:{r:?}"),
        }
    }
    // 顺手清掉这个实例的 drop-in(装自启时写的 User/HOME 指回)
    let _ = std::fs::remove_dir_all(format!("/etc/systemd/system/{inst}.service.d"));
    migrate_legacy(kind);
    if !PanKind::all().into_iter().any(instance_enabled) {
        let _ = std::fs::remove_file(TEMPLATE_PATH);
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        println!("所有网盘的自启都已关闭,模板服务已清理。");
    } else {
        println!("{} 自启已关闭。", kind.label());
    }
}

/// 老版本每个网盘一份独立服务(bdfs / bdfs-wopan,参数烤死在里面)。
/// 检测到就停用并删除,由模板实例接手(挂载点不变)
fn migrate_legacy(kind: PanKind) {
    if !legacy_unit_installed(kind) {
        return;
    }
    let old = legacy_unit(kind);
    println!("检测到旧版服务 {old}.service,迁移到模板实例 {}…", unit_instance(kind));
    let _ = Command::new("systemctl").args(["disable", "--now", old]).status();
    let _ = std::fs::remove_file(format!("/etc/systemd/system/{old}.service"));
    let _ = Command::new("systemctl").arg("daemon-reload").status();
}

/// 设置:逐项问,回车保持当前值(改的是当前网盘那份)
fn configure(kind: PanKind) {
    let mut cfg = Config::load();
    let st = cfg.settings_for_mut(kind);
    println!("-- {}设置(回车 = 保持当前值)--", kind.label());
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
    if let Err(e) = cfg.save() {
        println!("保存失败:{e:#}");
    }
}

// ---------- 小工具 ----------

/// 传输进度:读挂载进程写的进度文件(按网盘区分)
fn progress_view(kind: PanKind) {
    use crate::progress::ProgressFile;
    let p = pan::progress_file(kind);
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(_) => {
            println!("没有进度记录(挂载没在跑,或还没传过文件)。");
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
        println!("还没有传输活动。");
        return;
    }
    let now = crate::progress::now_ms();
    // 进度停更多半是挂载进程没了/卡住,提示而不是干等
    let stale = now.saturating_sub(v.updated_ms);
    if stale > 5000 {
        println!("(注意:进度已 {} 秒没更新,挂载进程可能没在跑)", stale / 1000);
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

/// 凭据状态:看各网盘自己的凭据文件在不在(不实际调 API)
fn creds_saved(kind: PanKind) -> bool {
    let dir = pan::config_dir();
    match kind {
        PanKind::Baidu => dir.join("token.json").is_file() && dir.join("config.json").is_file(),
        PanKind::Wopan => pan::wopan::token_saved(),
    }
}

fn cred_state(kind: PanKind) -> String {
    if creds_saved(kind) { "已保存".into() } else { "未配置".into() }
}

fn mount_state(mp: &str) -> String {
    if crate::fs::is_mounted(mp) {
        format!("已挂载({mp})")
    } else {
        "未挂载".into()
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// sudo 运行时找回真实用户(getent passwd:name:x:uid:gid:…:home)。
/// 非 sudo 的真 root 场景返回 None,实例就以 root 跑
fn sudo_user() -> Option<SudoUser> {
    let name = std::env::var("SUDO_USER").ok()?;
    if name.is_empty() || name == "root" {
        return None;
    }
    let out = Command::new("getent").args(["passwd", &name]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    let f: Vec<&str> = line.split(':').collect();
    Some(SudoUser {
        name: f.first()?.to_string(),
        uid: f.get(2)?.parse().ok()?,
        gid: f.get(3)?.parse().ok()?,
        home: f.get(5)?.to_string(),
    })
}

struct SudoUser {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// fuse.conf 是否已放行 user_allow_other
fn fuse_conf_allows_user_allow_other() -> bool {
    std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim_start().starts_with("user_allow_other")))
        .unwrap_or(false)
}

/// /etc/fuse.conf 开 user_allow_other:不开的话普通用户(和 User= 的自启实例)
/// 用 allow_other 挂载会被 fusermount3 拒。正在 root 下,问一句直接写
fn ensure_user_allow_other() {
    if fuse_conf_allows_user_allow_other() {
        return;
    }
    let Some(a) = ask(
        "/etc/fuse.conf 还没开 user_allow_other(不开:普通用户挂载 + allow_other 会被拒)。现在加上?[Y/n]",
    ) else {
        return;
    };
    if a.eq_ignore_ascii_case("n") {
        println!("跳过。之后手动在 /etc/fuse.conf 加一行 user_allow_other 即可。");
        return;
    }
    let mut s = std::fs::read_to_string("/etc/fuse.conf").unwrap_or_default();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("# added by bdfs:允许用户挂载使用 allow_other\nuser_allow_other\n");
    match std::fs::write("/etc/fuse.conf", s) {
        Ok(()) => println!("已写入 /etc/fuse.conf。"),
        Err(e) => println!("写 /etc/fuse.conf 失败:{e}(手动加一行 user_allow_other)"),
    }
}

/// 找出指定网盘的所有挂载进程(命令行是 bdfs/baidupan-fuse 且带 mount 子命令、
/// --backend 匹配),排除自己。自己扫 /proc 按 pid 杀,比 `pkill -f` 安全——
/// 后者会误杀命令行里恰好含同样字符串的进程(比如正在跑这条命令的 ssh 会话本身)。
/// 注意:没带 --backend 参数的进程是按默认网盘(百度,或设置里选中的)挂的,
/// 这里按"百度"算,老装机的 bdfs 服务不带 --backend,别漏杀
fn mount_pids(kind: PanKind, me: i32) -> Vec<i32> {
    let want_backend = format!("--backend={}", kind.id());
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
        if (exe != "bdfs" && exe != "baidupan-fuse")
            || !parts[1..].iter().any(|a| a == "mount")
        {
            continue;
        }
        // --backend wopan / --backend=wopan 两种写法都认
        let has_flag = parts
            .windows(2)
            .any(|w| w[0] == "--backend" && PanKind::parse(&w[1]) == Some(kind))
            || parts.contains(&want_backend);
        let no_flag = !parts.iter().any(|a| a.starts_with("--backend"));
        if has_flag || (no_flag && kind == PanKind::Baidu) {
            out.push(pid);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{short_label, template_body};
    use crate::pan::PanKind;
    use std::path::Path;

    /// 模板的关键约束:ExecStart 只带网盘参数(挂载参数启动时从设置实时读),
    /// 实例名用 %i 注入,失败要重启但留有间隔
    #[test]
    fn 模板单元形状() {
        let body = template_body(Path::new("/usr/local/bin/bdfs"));
        assert!(
            body.contains("--backend %i mount"),
            "ExecStart 应只带网盘参数:\n{body}"
        );
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("RestartSec="));
        assert!(body.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn 实例名与短名() {
        // 实例名/短名按注册表取值,防止顺手改坏状态栏
        assert_eq!(super::unit_instance(PanKind::Wopan), "bdfs@wopan");
        assert_eq!(super::unit_instance(PanKind::Baidu), "bdfs@baidu");
        assert_eq!(short_label(PanKind::Wopan), "联通");
        assert_eq!(short_label(PanKind::Baidu), "百度");
    }
}

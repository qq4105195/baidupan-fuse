//! 系统托盘(M6):OneDrive 式日常入口——右下角图标,右键菜单开/关同步、
//! 开机自启、设置、进度、退出,日常使用不用开终端。
//!
//! 形态是**子进程模型**:托盘进程管理一个隐藏的 `bdfs sync` 子进程——
//! 同步崩溃不连坐托盘;托盘被杀,子进程孤儿但继续工作(互斥+停止事件仍在),
//! 下次 `bdfs tray` 用 OpenMutexW 探到"外部同步"就接管显示。
//!
//! 两条硬规则:
//! - **FreeConsole 之后本进程零 println**(写已脱离的控制台句柄会 panic),
//!   失败要么吞掉要么追加写 tray.log
//! - 托盘/菜单/消息泵全在主线程(tray-icon 的对象全 !Send;它的隐藏窗口
//!   靠本线程的 PeekMessageW 泵驱动,不泵图标就死)

use super::{sync_running, wstr};
use crate::settings::Settings;
use anyhow::anyhow;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 托盘自己的单实例互斥(与 sync 的分开:允许"托盘在跑 + 别处又起 sync")
const TRAY_MUTEX: &str = r"Local\bdfs-tray-single";
// 菜单项 id(muda 的 MenuId 直接与 &str 比较)
const ID_OPEN: &str = "open-folder";
const ID_TOGGLE: &str = "toggle-sync";
const ID_AUTOSTART: &str = "autostart";
const ID_CONFIG: &str = "settings";
const ID_PROGRESS: &str = "progress";
const ID_QUIT: &str = "quit";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
/// 状态轮询间隔:子进程退出/外部同步出现,2s 内反映到菜单文案
const POLL: Duration = Duration::from_secs(2);

/// 同步子进程状态
enum SyncState {
    Stopped,
    /// 我们拉起的隐藏子进程(生命周期归我们管)
    Ours(Child),
    /// 别处起的同步(控制台/上次托盘残留的孤儿):能停,管不到退出
    External,
}

/// 托盘入口:单实例 → 建 UI(还在控制台上,报错可见)→ 脱离控制台 →
/// 自动带起同步 → 消息循环。次序承重,别调换。
///
/// `quiet = false`(手点/双击桌面图标):开一个资源管理器窗口到同步根——
/// OneDrive 式语义,双击桌面图标永远"打开网盘";托盘已在跑也一样。
/// `quiet = true`(登录自启):只起托盘,别每次登录都弹文件夹。
pub(crate) fn run(st: &Settings, quiet: bool) -> anyhow::Result<()> {
    let sync_root = super::resolve_sync_root(&st.sync_root);
    let first = claim_tray_instance()?;
    if !quiet {
        // explorer 即使成功也常返回 1,忽略状态
        let _ = Command::new("explorer").arg(&sync_root).spawn();
    }
    if !first {
        // 已有托盘:开完文件夹就退(这就是"双击打不开"的正解——不是失败)
        return Ok(());
    }
    let ui = TrayUi::build()?;
    println!("bdfs 托盘已启动(右键图标操作,退出用托盘菜单)。");
    println!("同步日志:%APPDATA%\\baidupan-fuse\\sync.log");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // 从这行起本进程不能再 println(见模块注释)
    unsafe { windows_sys::Win32::System::Console::FreeConsole() };

    let mut app = TrayApp {
        state: SyncState::Stopped,
        ui,
        sync_root,
        sync_root_setting: st.sync_root.clone(),
        settings_fp: settings_fingerprint(),
    };
    if sync_running() {
        app.state = SyncState::External;
    } else {
        app.start_sync();
    }
    app.paint_status();
    app.message_loop();
    app.stop_ours_graceful();
    Ok(())
}

// ---------- 托盘应用 ----------

struct TrayApp {
    state: SyncState,
    ui: TrayUi,
    /// 同步根(打开文件夹用)
    sync_root: PathBuf,
    /// 设置里的同步根串(stop() 的参数,留字段对齐将来按根区分事件)
    sync_root_setting: String,
    /// 子进程拉起时的 settings.json 指纹(轮询对比发现变更就重启它)
    settings_fp: u64,
}

impl TrayApp {
    /// 主线程单循环:Win32 消息泵 + 菜单事件 + 状态轮询,150ms 一拍
    fn message_loop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
        };
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let mut last_poll = Instant::now();
        'outer: loop {
            unsafe {
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        break 'outer;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if ev.id == ID_OPEN {
                    self.open_folder();
                } else if ev.id == ID_TOGGLE {
                    match self.state {
                        SyncState::Stopped => self.start_sync(),
                        _ => self.stop_clicked(),
                    }
                    self.paint_status();
                } else if ev.id == ID_AUTOSTART {
                    // CheckMenuItem 点击时平台已把勾翻好:is_checked() 是新值,
                    // 照它写注册表,再以注册表为准回填(写失败勾会弹回去)
                    let on = self.ui.autostart.is_checked();
                    let r = if on {
                        crate::menu::autostart_enable().map(|_| ())
                    } else {
                        crate::menu::autostart_disable()
                    };
                    if let Err(e) = r {
                        tray_log(&format!("写开机自启注册表失败:{e:#}"));
                    }
                    let _ = self
                        .ui
                        .autostart
                        .set_checked(crate::menu::autostart_exists());
                } else if ev.id == ID_CONFIG {
                    // 设置是 GUI 窗口,别带控制台(否则会闪一个黑框)
                    spawn_sub("config", false);
                } else if ev.id == ID_PROGRESS {
                    spawn_sub("progress", true);
                } else if ev.id == ID_QUIT {
                    self.stop_ours_graceful();
                    break 'outer;
                }
            }
            if last_poll.elapsed() >= POLL {
                self.refresh_status();
                // 设置文件变了 → 优雅重启子进程吃新配置(设置窗口保存后自动生效;
                // 只管自己拉起的,外部同步让主人自己重启)
                if matches!(self.state, SyncState::Ours(_)) {
                    let fp = settings_fingerprint();
                    if fp != self.settings_fp {
                        tray_log("settings.json 变更,重启同步子进程");
                        self.stop_clicked();
                        self.start_sync();
                        self.paint_status();
                    }
                }
                last_poll = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// 起同步子进程(隐藏窗口,stdout/stderr 进 sync.log)
    fn start_sync(&mut self) {
        if !matches!(self.state, SyncState::Stopped) {
            return;
        }
        match spawn_sync() {
            Ok(c) => {
                self.state = SyncState::Ours(c);
                // 子进程自己 Settings::load,指纹按"它即将读到的"为准
                self.settings_fp = settings_fingerprint();
            }
            Err(e) => tray_log(&format!("起同步子进程失败:{e}")),
        }
    }

    /// 菜单"停止":命名事件谁家的同步都停;自己的子进程等它退完
    fn stop_clicked(&mut self) {
        super::stop(&self.sync_root_setting);
        if matches!(self.state, SyncState::Ours(_)) {
            self.wait_ours();
        }
        // External 的进程收到事件后自退,refresh_status 看到互斥消失再翻文案
    }

    /// 退出路径:只停自己拉起的子进程(外部起的 sync 不归托盘管)
    fn stop_ours_graceful(&mut self) {
        if matches!(self.state, SyncState::Ours(_)) {
            super::stop(&self.sync_root_setting);
            self.wait_ours();
        }
    }

    /// 等自己的子进程退(200ms 步进 ≤5s,超时 kill+wait——Child drop
    /// 既不杀也不收尸,必须显式 reap 防僵尸)
    fn wait_ours(&mut self) {
        let SyncState::Ours(mut c) =
            std::mem::replace(&mut self.state, SyncState::Stopped)
        else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match c.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                Ok(None) => {
                    let _ = c.kill();
                    let _ = c.wait();
                    break;
                }
            }
        }
    }

    /// 2s 一拍的状态机:子进程退出/外部同步出现或消失 → 刷新菜单文案
    fn refresh_status(&mut self) {
        // 先算迁移再赋值(避免持有 state 可变借用的同时改 state)
        let to_stopped = match &mut self.state {
            SyncState::Ours(c) => c.try_wait().ok().flatten().is_some(),
            SyncState::External => !sync_running(),
            SyncState::Stopped => false,
        };
        if to_stopped {
            self.state = SyncState::Stopped;
        } else if matches!(self.state, SyncState::Stopped) && sync_running() {
            self.state = SyncState::External;
        }
        self.paint_status();
    }

    /// 把状态刷到菜单项文案 + 托盘 tooltip
    fn paint_status(&self) {
        let running = !matches!(self.state, SyncState::Stopped);
        let text = if running {
            "同步:运行中(点击停止)"
        } else {
            "同步:已停止(点击启动)"
        };
        let _ = self.ui.toggle.set_text(text);
        let tip = if running {
            "bdfs 同步:运行中"
        } else {
            "bdfs 同步:已停止"
        };
        let _ = self.ui.tray.set_tooltip(Some(tip));
    }

    fn open_folder(&self) {
        // explorer 即使成功也常返回 1,忽略状态
        let _ = Command::new("explorer").arg(&self.sync_root).spawn();
    }
}

// ---------- 托盘 UI ----------

/// 托盘图标 + 需要动态更新的菜单项句柄(其余项建完由 Menu 保活)
struct TrayUi {
    tray: tray_icon::TrayIcon,
    /// 开关同步项(文案随状态变)
    toggle: tray_icon::menu::MenuItem,
    /// 自启勾选项
    autostart: tray_icon::menu::CheckMenuItem,
}

impl TrayUi {
    fn build() -> anyhow::Result<Self> {
        use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
        let open = MenuItem::with_id(ID_OPEN, "打开「百度网盘」文件夹", true, None);
        let toggle = MenuItem::with_id(ID_TOGGLE, "同步:已停止(点击启动)", true, None);
        let autostart = CheckMenuItem::with_id(
            ID_AUTOSTART,
            "开机自动同步(托盘)",
            true,
            crate::menu::autostart_exists(),
            None,
        );
        let config = MenuItem::with_id(ID_CONFIG, "设置…", true, None);
        let progress = MenuItem::with_id(ID_PROGRESS, "传输进度…", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "退出", true, None);

        let menu = Menu::new();
        menu.append(&open)
            .and_then(|_| menu.append(&toggle))
            .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
            .and_then(|_| menu.append(&autostart))
            .and_then(|_| menu.append(&config))
            .and_then(|_| menu.append(&progress))
            .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
            .and_then(|_| menu.append(&quit))
            .map_err(|e| anyhow!("托盘菜单拼装失败:{e}"))?;

        let tray = tray_icon::TrayIconBuilder::new()
            .with_icon(build_icon())
            .with_tooltip("bdfs 同步")
            .with_menu(Box::new(menu))
            // 左键不弹菜单(默认会弹),只认右键
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| anyhow!("建托盘图标失败:{e}"))?;
        Ok(Self {
            tray,
            toggle,
            autostart,
        })
    }
}

/// 32×32 蓝云朵,纯代码画(零资源文件):三圆并集 + 底部拉平
fn build_icon() -> tray_icon::Icon {
    const W: i32 = 32;
    let mut buf = vec![0u8; (W * W * 4) as usize]; // 全 0 = 全透明
    let discs: [(f32, f32, f32); 3] =
        [(11.0, 20.0, 6.0), (17.0, 14.0, 6.5), (23.0, 20.0, 5.0)];
    for y in 0..W {
        for x in 0..W {
            let in_disc = discs.iter().any(|&(cx, cy, rad)| {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                dx * dx + dy * dy <= rad * rad
            });
            let in_base = (11..=24).contains(&x) && (19..=25).contains(&y);
            if in_disc || in_base {
                let i = ((y * W + x) * 4) as usize;
                buf[i] = 59; // #3B82F6,深浅任务栏上都读得清
                buf[i + 1] = 130;
                buf[i + 2] = 246;
                buf[i + 3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(buf, W as u32, W as u32).expect("32×32×4 构造即合法")
}

// ---------- 子进程与工具 ----------

/// 起隐藏同步子进程:无参 `sync`(子进程自己 Settings::load,托盘改设置后
/// 重启即生效);stdout/stderr 截断写 sync.log(每次启动翻新)。
/// 与外部 sync 撞车 → 子进程撞单实例互斥退 1,refresh_status 自愈成 External
fn spawn_sync() -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    let dir = crate::baidu::config_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("sync.log"))?;
    Command::new(std::env::current_exe()?)
        .arg("sync")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

/// 托盘拉子命令:设置是 GUI 窗口(不带控制台,免闪黑框);进度仍是控制台窗口
fn spawn_sub(sub: &str, console: bool) {
    use std::os::windows::process::CommandExt;
    let flags = if console {
        CREATE_NEW_CONSOLE
    } else {
        CREATE_NO_WINDOW
    };
    let r = Command::new(std::env::current_exe().unwrap_or_default())
        .arg(sub)
        .creation_flags(flags)
        .spawn();
    if let Err(e) = r {
        tray_log(&format!("拉起 bdfs {sub} 失败:{e}"));
    }
}

/// settings.json 内容指纹:设置窗口保存后托盘发现变更,自动重启同步生效
fn settings_fingerprint() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(crate::baidu::config_dir().join("settings.json"))
        .unwrap_or_default()
        .hash(&mut h);
    h.finish()
}

/// 托盘没有 stdout(FreeConsole 后 println 会 panic),出错追加写这
fn tray_log(line: &str) {
    use std::io::Write;
    let dir = crate::baidu::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tray.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 托盘单实例(命名互斥)。false = 已经有一个在跑(不是错误,见 run);
/// 报错在 FreeConsole 前打,用户看得见
fn claim_tray_instance() -> anyhow::Result<bool> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let m = unsafe { CreateMutexW(std::ptr::null(), 0, wstr(TRAY_MUTEX).as_ptr()) };
    if !m.is_null() && unsafe { GetLastError() } == 183 {
        return Ok(false);
    }
    if m.is_null() {
        anyhow::bail!("建托盘互斥失败:{}", std::io::Error::last_os_error());
    }
    // 句柄故意不 CloseHandle(裸指针无 RAII,不关即持有),进程退出系统回收
    Ok(true)
}

//! 设置窗口(M7):托盘「设置…」弹出的原生 Win32 对话框,替代控制台问答。
//! 两个区块:**登录授权**(AppKey/AppSecret → 授权网址 → 粘授权码换 token)和
//! **同步设置**(同步根/远端根/目录/直链缓存秒)。
//!
//! 形态:`bdfs config` 子命令 = 本窗口。托盘用 CREATE_NO_WINDOW 拉起它(纯 GUI
//! 不闪控制台),自带消息循环,关窗进程即退;裸跑控制台的「7. 设置」也是同一窗。
//! 保存写 settings.json——托盘每 2s 对内容做指纹,变了就自动重启同步子进程,
//! 所以保存即生效,不用手动重启同步。
//!
//! 实现注:不引 egui(拖几十个 crate),纯 windows-sys 手搓,系统原生观感。
//! 单窗口单线程;控件 HWND 装箱挂 GWLP_USERDATA,窗口过程零全局可变状态。
//! 坐标按 96DPI 手排(高分屏由系统整体缩放,接受);字体微软雅黑 9pt。
//! 换 token 是一次阻塞 HTTP(超时 60s),期间窗口短暂无响应,v1 接受。

use super::wstr;
use crate::settings::Settings;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BS_DEFPUSHBUTTON, CreateWindowExW, CREATESTRUCTW, CW_USEDEFAULT,
    DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, ES_NUMBER, ES_PASSWORD,
    GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GWLP_USERDATA, HMENU, IDC_ARROW, IDI_APPLICATION, IsDialogMessageW, LoadCursorW, LoadIconW,
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW, MSG, PostQuitMessage,
    RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, SW_SHOW,
    ShowWindow, TranslateMessage, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY,
    WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE, WS_MAXIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_THICKFRAME, WS_VISIBLE,
};

// 控件 id:保存/取消直接用系统 IDOK/IDCANCEL,IsDialogMessageW 的
// 回车(默认钮)/ESC(取消)就会落到同一条 WM_COMMAND 分支
const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const IDC_LOGIN_STATE: usize = 1000;
const IDC_APPKEY: usize = 1001;
const IDC_APPSECRET: usize = 1002;
const IDC_LOGIN_START: usize = 1003;
const IDC_AUTH_URL: usize = 1004;
const IDC_AUTH_CODE: usize = 1005;
const IDC_LOGIN_DONE: usize = 1006;
const IDC_SYNC: usize = 1007;
const IDC_BROWSE: usize = 1008;
const IDC_ROOT: usize = 1009;
const IDC_DIR_TTL: usize = 1010;
const IDC_LINK_TTL: usize = 1011;

const CLASS: &str = "bdfs_config_wnd";
const TITLE: &str = "bdfs 设置";

/// SS_ETCHEDHORZ(windows-sys 没导这个常量,字面量):凹槽分隔线
const SS_ETCHEDHORZ: u32 = 0x10;
/// cmd /c start 拉浏览器时不许闪控制台窗
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 编辑框初始值(run() 装好,WM_CREATE 取;只读共享无需锁)
struct Initial {
    appkey: Vec<u16>,
    sync_root: Vec<u16>,
    root: Vec<u16>,
    dir_ttl: Vec<u16>,
    link_ttl: Vec<u16>,
}
static INITIAL: OnceLock<Initial> = OnceLock::new();

/// 控件字体句柄(usize 存裸指针绕开 !Sync;进程单窗口,不释放)
static FONT: OnceLock<usize> = OnceLock::new();

/// 控件句柄包(堆上,挂 GWLP_USERDATA;WM_DESTROY 收回)
struct Controls {
    state_label: HWND,
    appkey_edit: HWND,
    appsecret_edit: HWND,
    url_edit: HWND,
    code_edit: HWND,
    sync_edit: HWND,
    root_edit: HWND,
    dir_ttl_edit: HWND,
    link_ttl_edit: HWND,
}

/// 弹设置窗口,阻塞到关窗;保存写 settings.json(托盘侧自动重启同步生效)
pub(crate) fn run() -> anyhow::Result<()> {
    let st = Settings::load();
    // AppSecret 刻意不回填(不在界面上展示已保存的密钥),留空 = 沿用
    let appkey = crate::baidu::AppConfig::load()
        .map(|c| c.app_key)
        .unwrap_or_default();
    let _ = INITIAL.set(Initial {
        appkey: wstr(&appkey),
        sync_root: wstr(&st.sync_root),
        root: wstr(&st.root),
        dir_ttl: wstr(&st.dir_ttl.to_string()),
        link_ttl: wstr(&st.dlink_ttl.to_string()),
    });

    let hwnd = unsafe {
        use windows_sys::Win32::Graphics::Gdi::{CreateFontW, GetSysColorBrush, COLOR_BTNFACE};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let _ = FONT.set(
            CreateFontW(
                -12, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0,
                wstr("Microsoft YaHei UI").as_ptr(),
            ) as usize,
        );

        let class = wstr(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("注册窗口类失败:{}", std::io::Error::last_os_error());
        }

        // 固定大小窗口(去掉缩放边和最大化钮);客户区 480×452 折算外框
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 480,
            bottom: 452,
        };
        AdjustWindowRect(&mut rc, style, 0);
        CreateWindowExW(
            0,
            class.as_ptr(),
            wstr(TITLE).as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            std::ptr::null_mut(),
            0 as HMENU,
            wc.hInstance,
            INITIAL.get().unwrap() as *const Initial as *mut core::ffi::c_void,
        )
    };
    if hwnd.is_null() {
        anyhow::bail!("创建窗口失败:{}", std::io::Error::last_os_error());
    }
    unsafe {
        // 连打两次:进程带 STARTUPINFO(如 -WindowStyle Hidden 拉的)时首个
        // ShowWindow 会被它的 wShowWindow 顶掉,第二次才是自己说了算
        ShowWindow(hwnd, SW_SHOW);
        ShowWindow(hwnd, SW_SHOW);
        // 托盘场景是后台 CREATE_NO_WINDOW 拉的,得自己抢前台才看得见
        SetForegroundWindow(hwnd);
    }

    // 消息循环:IsDialogMessageW 白送 TAB 导航 / 回车=默认钮 / ESC=取消
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

// ---------- 窗口过程 ----------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => on_create(hwnd, &*(lp as *const CREATESTRUCTW)),
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as usize;
            let ctl = controls_of(hwnd);
            match id {
                IDOK if !ctl.is_null() => {
                    on_save(hwnd, &*ctl);
                    0
                }
                IDCANCEL => {
                    DestroyWindow(hwnd);
                    0
                }
                IDC_LOGIN_START if !ctl.is_null() => {
                    on_login_start(hwnd, &*ctl);
                    0
                }
                IDC_LOGIN_DONE if !ctl.is_null() => {
                    on_login_done(hwnd, &*ctl);
                    0
                }
                IDC_BROWSE if !ctl.is_null() => {
                    on_browse(hwnd, (*ctl).sync_edit);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        // 静态文本底色对齐客户区灰底(不处理会刷成窗口底色,字底发白)
        WM_CTLCOLORSTATIC => {
            use windows_sys::Win32::Graphics::Gdi::{
                GetSysColor, GetSysColorBrush, SetBkColor, COLOR_BTNFACE,
            };
            SetBkColor(wp as *mut core::ffi::c_void, GetSysColor(COLOR_BTNFACE));
            GetSysColorBrush(COLOR_BTNFACE) as LRESULT
        }
        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Controls;
            if !p.is_null() {
                drop(Box::from_raw(p));
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 取控件包指针(WM_COMMAND 时已就位;WM_CREATE 之前是 null)
unsafe fn controls_of(hwnd: HWND) -> *mut Controls {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Controls
}

/// 建控件 + 存句柄;坐标 96DPI 手排(见模块注释)。
/// 上半 = 登录授权,分隔线,下半 = 同步设置
unsafe fn on_create(hwnd: HWND, cs: &CREATESTRUCTW) -> LRESULT {
    let init = &*(cs.lpCreateParams as *const Initial);
    let font = FONT.get().copied().unwrap_or(0);

    // ---- 登录授权 ----
    label(hwnd, wstr("登录状态"), 16, 18, 88, 20, font);
    let state_label = child(
        hwnd,
        "STATIC",
        &wstr(&login_state_text()),
        0,
        0,
        110,
        18,
        338,
        20,
        IDC_LOGIN_STATE,
        font,
    );
    label(hwnd, wstr("AppKey"), 16, 48, 88, 20, font);
    let appkey_edit = edit(hwnd, &init.appkey, IDC_APPKEY, 110, 45, 338, 24, font, false, false);
    label(hwnd, wstr("AppSecret"), 16, 78, 88, 20, font);
    let appsecret_edit = edit(
        hwnd,
        &wstr(""),
        IDC_APPSECRET,
        110,
        75,
        338,
        24,
        font,
        false,
        true,
    );
    button(hwnd, &wstr("开始授权"), IDC_LOGIN_START, 110, 106, 88, 28, font, false);
    label(hwnd, wstr("Secret 留空 = 沿用已保存"), 210, 110, 238, 18, font);
    label(hwnd, wstr("授权网址"), 16, 142, 88, 20, font);
    let url_edit = edit(
        hwnd,
        &wstr(""),
        IDC_AUTH_URL,
        110,
        139,
        338,
        24,
        font,
        false,
        false,
    );
    label(hwnd, wstr("授权码"), 16, 172, 88, 20, font);
    let code_edit = edit(hwnd, &wstr(""), IDC_AUTH_CODE, 110, 169, 338, 24, font, false, false);
    button(hwnd, &wstr("完成授权"), IDC_LOGIN_DONE, 110, 200, 88, 28, font, false);
    // 只读编辑框(授权网址):可选中复制,不让手改
    SendMessageW(
        url_edit,
        0x00CF, // EM_SETREADONLY
        1,
        0,
    );
    // 分隔线(文本必须 NUL 结尾:wstr("") 而不是空切片,否则按指针找 NUL 会越界)
    child(
        hwnd,
        "STATIC",
        &wstr(""),
        0,
        SS_ETCHEDHORZ,
        16,
        240,
        448,
        2,
        0,
        font,
    );

    // ---- 同步设置 ----
    label(hwnd, wstr("同步根目录"), 16, 254, 88, 20, font);
    let sync_edit = edit(hwnd, &init.sync_root, IDC_SYNC, 110, 251, 270, 24, font, false, false);
    button(hwnd, &wstr("浏览…"), IDC_BROWSE, 388, 249, 60, 28, font, false);

    label(hwnd, wstr("远端根目录"), 16, 294, 88, 20, font);
    let root_edit = edit(hwnd, &init.root, IDC_ROOT, 110, 291, 338, 24, font, false, false);
    label(
        hwnd,
        wstr("未过审应用只能访问 /apps/<应用名>;挂全盘填 /"),
        110,
        320,
        340,
        18,
        font,
    );

    label(hwnd, wstr("目录缓存秒"), 16, 354, 88, 20, font);
    let dir_ttl_edit = edit(
        hwnd,
        &init.dir_ttl,
        IDC_DIR_TTL,
        110,
        351,
        90,
        24,
        font,
        true,
        false,
    );
    label(hwnd, wstr("直链缓存秒"), 232, 354, 88, 20, font);
    let link_ttl_edit = edit(
        hwnd,
        &init.link_ttl,
        IDC_LINK_TTL,
        330,
        351,
        90,
        24,
        font,
        true,
        false,
    );

    button(hwnd, &wstr("保存"), IDOK, 262, 400, 88, 30, font, true);
    button(hwnd, &wstr("取消"), IDCANCEL, 360, 400, 84, 30, font, false);

    SetWindowLongPtrW(
        hwnd,
        GWLP_USERDATA,
        Box::into_raw(Box::new(Controls {
            state_label,
            appkey_edit,
            appsecret_edit,
            url_edit,
            code_edit,
            sync_edit,
            root_edit,
            dir_ttl_edit,
            link_ttl_edit,
        })) as isize,
    );
    SetFocus(sync_edit);
    0
}

// ---------- 事件处理 ----------

/// 开始授权:存凭据(Secret 留空沿用旧的)→ 生成授权网址 → 唤起浏览器。
/// 不打任何 API;真正换 token 在「完成授权」
unsafe fn on_login_start(hwnd: HWND, c: &Controls) {
    let ak = text_of(c.appkey_edit).trim().to_string();
    if ak.is_empty() {
        msgbox(hwnd, "AppKey 不能为空。", true);
        return;
    }
    let sk_typed = text_of(c.appsecret_edit);
    let sk_typed = sk_typed.trim().to_string();
    let secret = if !sk_typed.is_empty() {
        sk_typed
    } else {
        match crate::baidu::AppConfig::load() {
            // Secret 属于具体 AppKey:留空沿用只对同一个 Key 成立
            Ok(saved) if saved.app_key == ak => saved.app_secret,
            _ => {
                msgbox(hwnd, "AppSecret 为空,且没有可沿用的已保存凭据(AppKey 也对不上)。", true);
                return;
            }
        }
    };
    let new_cfg = crate::baidu::AppConfig {
        app_key: ak.clone(),
        app_secret: secret,
    };
    if let Err(e) = new_cfg.save() {
        msgbox(hwnd, &format!("保存应用凭据失败:{e:#}"), true);
        return;
    }
    let url = crate::baidu::authcode_url(&ak);
    SetWindowTextW(c.url_edit, wstr(&url).as_ptr());
    // 浏览器打开:start 的空标题参数防 URL 被当窗口名
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    set_state(
        c.state_label,
        "浏览器已打开,同意授权后把页面显示的授权码粘到下面",
    );
}

/// 完成授权:授权码换 token(一次阻塞 HTTP,超时 60s,窗口会短暂无响应)
unsafe fn on_login_done(hwnd: HWND, c: &Controls) {
    let code = text_of(c.code_edit).trim().to_string();
    if code.is_empty() {
        msgbox(hwnd, "授权码不能为空(先点「开始授权」,浏览器同意后页面会显示)。", true);
        return;
    }
    let cfg = match crate::baidu::AppConfig::load() {
        Ok(v) => v,
        Err(e) => {
            msgbox(hwnd, &format!("读取应用凭据失败:{e:#}"), true);
            return;
        }
    };
    match crate::baidu::authcode_exchange(&cfg.app_key, &cfg.app_secret, &code) {
        Ok(_) => {
            set_state(c.state_label, "登录成功,token 已保存");
            msgbox(hwnd, "授权成功,登录完成。", false);
        }
        Err(e) => {
            set_state(c.state_label, "授权失败,可重试");
            msgbox(hwnd, &format!("授权失败:{e:#}"), true);
        }
    }
}

/// 保存:校验 → 写 settings.json(只动这四项,其余字段以磁盘为准)→ 关窗
unsafe fn on_save(hwnd: HWND, c: &Controls) {
    let sync_root = text_of(c.sync_edit).trim().to_string();
    if sync_root.is_empty() {
        msgbox(hwnd, "同步根目录不能为空。", true);
        return;
    }
    let root = text_of(c.root_edit);
    let root = {
        let t = root.trim();
        if t.is_empty() {
            "/".to_string()
        } else {
            t.to_string()
        }
    };
    let dir_ttl = match text_of(c.dir_ttl_edit).trim().parse::<u64>() {
        Ok(n) if n > 0 => n,
        _ => {
            msgbox(hwnd, "目录列表缓存秒数要是 ≥1 的整数。", true);
            return;
        }
    };
    let dlink_ttl = match text_of(c.link_ttl_edit).trim().parse::<u64>() {
        Ok(n) if n > 0 => n,
        _ => {
            msgbox(hwnd, "下载直链缓存秒数要是 ≥1 的整数。", true);
            return;
        }
    };
    let mut st = Settings::load();
    st.sync_root = sync_root;
    st.root = root;
    st.dir_ttl = dir_ttl;
    st.dlink_ttl = dlink_ttl;
    if let Err(e) = st.save() {
        msgbox(hwnd, &format!("保存失败:{e:#}"), true);
        return;
    }
    msgbox(hwnd, "已保存。正在运行的同步会自动重启生效(几秒内)。", false);
    DestroyWindow(hwnd);
}

/// 目录选择:老式 SHBrowseForFolder(不碰 COM 初始化,取消返回空 pidl)
unsafe fn on_browse(owner: HWND, target: HWND) {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
    };
    let title = wstr("选择同步根目录");
    let mut bi: BROWSEINFOW = std::mem::zeroed();
    bi.hwndOwner = owner;
    bi.lpszTitle = title.as_ptr();
    bi.ulFlags = BIF_RETURNONLYFSDIRS;
    let pidl = SHBrowseForFolderW(&mut bi);
    if pidl.is_null() {
        return; // 用户取消
    }
    let mut buf = [0u16; 520];
    if SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0 {
        let s = String::from_utf16_lossy(&buf);
        let _ = SetWindowTextW(target, wstr(s.trim_end_matches('\0')).as_ptr());
    }
    CoTaskMemFree(pidl as *mut core::ffi::c_void);
}

// ---------- 控件小工具 ----------

/// 通用子控件创建(系统类 STATIC/EDIT/BUTTON,不挑实例句柄)
unsafe fn child(
    parent: HWND,
    class: &str,
    text: &[u16],
    ex: u32,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
    font: usize,
) -> HWND {
    let h = CreateWindowExW(
        ex,
        wstr(class).as_ptr(),
        text.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        w,
        h,
        parent,
        id as HMENU,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if font != 0 {
        SendMessageW(h, WM_SETFONT, font, 1);
    }
    h
}

unsafe fn label(parent: HWND, text: Vec<u16>, x: i32, y: i32, w: i32, h: i32, font: usize) -> HWND {
    child(parent, "STATIC", &text, 0, 0, x, y, w, h, 0, font)
}

/// 单行编辑框;number_only → ES_NUMBER 只让输数字;password → ES_PASSWORD 掩码
unsafe fn edit(
    parent: HWND,
    text: &[u16],
    id: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: usize,
    number_only: bool,
    password: bool,
) -> HWND {
    let mut style = WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32;
    if number_only {
        style |= ES_NUMBER as u32;
    }
    if password {
        style |= ES_PASSWORD as u32;
    }
    child(parent, "EDIT", text, WS_EX_CLIENTEDGE, style, x, y, w, h, id, font)
}

unsafe fn button(
    parent: HWND,
    text: &[u16],
    id: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: usize,
    default_btn: bool,
) -> HWND {
    let mut style = WS_TABSTOP;
    if default_btn {
        style |= BS_DEFPUSHBUTTON as u32;
    }
    child(parent, "BUTTON", text, 0, style, x, y, w, h, id, font)
}

unsafe fn text_of(h: HWND) -> String {
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), len + 1);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn set_state(label_h: HWND, text: &str) {
    SetWindowTextW(label_h, wstr(text).as_ptr());
}

/// 登录状态文案:token.json + config.json 都在就算已登录(不调 API 验证,省配额)
fn login_state_text() -> String {
    let dir = crate::baidu::config_dir();
    let has = dir.join("token.json").is_file() && dir.join("config.json").is_file();
    if has {
        "已登录(token 已保存)".to_string()
    } else {
        "未配置(填 AppKey/Secret,点「开始授权」)".to_string()
    }
}

unsafe fn msgbox(hwnd: HWND, text: &str, err: bool) {
    let icon = if err {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    MessageBoxW(
        hwnd,
        wstr(text).as_ptr(),
        wstr(TITLE).as_ptr(),
        icon | MB_OK | MB_SETFOREGROUND,
    );
}

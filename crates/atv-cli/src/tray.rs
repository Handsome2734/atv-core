//! Windows system-tray front-end: hides the console and exposes a tray icon with
//! a right-click menu (open web console, trackpad mode, start at login, log, quit).

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use atv_core::TrackpadMode;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub enum TrayCmd {
    Mode(TrackpadMode),
    ToggleService,
    Quit,
}

struct State {
    tx: UnboundedSender<TrayCmd>,
    mode: Arc<AtomicU8>,
    url: Option<String>,
    title: String,
    running: Arc<std::sync::atomic::AtomicBool>,
}

static STATE: OnceLock<State> = OnceLock::new();

const WM_TRAY: u32 = WM_APP + 1;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "AppleTVRemote";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Copy `s` into a fixed-size, NUL-terminated UTF-16 buffer.
fn fill(buf: &mut [u16], s: &str) {
    let max = buf.len().saturating_sub(1);
    for (d, c) in buf[..max].iter_mut().zip(s.encode_utf16()) {
        *d = c;
    }
}

fn hidden(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn log_path() -> std::path::PathBuf {
    atv_core::settings::get_settings_file_path().with_file_name("atv-cli.log")
}

fn autostart_enabled() -> bool {
    hidden("reg", &["query", RUN_KEY, "/v", RUN_NAME])
}

fn set_autostart(on: bool) {
    if on {
        let exe = std::env::current_exe().unwrap_or_default();
        let val = format!("\"{}\"", exe.display());
        hidden("reg", &["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &val, "/f"]);
    } else {
        hidden("reg", &["delete", RUN_KEY, "/v", RUN_NAME, "/f"]);
    }
}

pub fn message_box(text: &str) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("Apple TV Remote").as_ptr(),
            MB_ICONINFORMATION,
        );
    }
}

/// True if another instance already holds the app mutex (handle is intentionally leaked).
pub fn already_running() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    unsafe {
        CreateMutexW(std::ptr::null(), 0, wide("Local\\AppleTVRemoteSingleton").as_ptr());
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

pub fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn open_console(s: &State) {
    if let Some(url) = &s.url {
        open_url(url);
    }
}


fn show_menu(hwnd: HWND, s: &State) {
    let is_running = s.running.load(Ordering::SeqCst);
    let status_str = if is_running {
        format!("● 服务运行中 · {}", s.title)
    } else {
        "○ 服务已停止 (Service Stopped)".to_string()
    };
    let toggle_action_str = if is_running {
        "⏹ 停止模拟服务 (Stop Service)"
    } else {
        "▶ 启动模拟服务 (Start Service)"
    };

    let mode = TrackpadMode::from_u8(s.mode.load(Ordering::SeqCst));
    let check = |m: TrackpadMode| if m == mode { MF_CHECKED } else { 0 };
    let mode_flags = if is_running { 0 } else { MF_GRAYED };
    let auto = if autostart_enabled() { MF_CHECKED } else { 0 };

    let items: [(u32, usize, String); 13] = [
        (MF_GRAYED, 0, status_str),
        (MF_SEPARATOR, 0, String::new()),
        (0, 10, toggle_action_str.to_string()),
        (if is_running { 0 } else { MF_GRAYED }, 1, "打开 Web 控制台 (Open Console)".to_string()),
        (MF_SEPARATOR, 0, String::new()),
        (mode_flags | check(TrackpadMode::Mouse), 2, "🖱️ 鼠标光标模式 (Mouse)".to_string()),
        (mode_flags | check(TrackpadMode::Direction), 3, "◀▲▼▶ 方向键模式 (D-pad)".to_string()),
        (mode_flags | check(TrackpadMode::Idle), 4, "👁️ 触控板空置模式 (Idle)".to_string()),
        (MF_SEPARATOR, 0, String::new()),
        (auto, 5, "开机自启动 (Start at login)".to_string()),
        (0, 6, "打开日志文件 (Open log)".to_string()),
        (MF_SEPARATOR, 0, String::new()),
        (0, 9, "退出程序 (Quit)".to_string()),
    ];
    unsafe {
        let menu = CreatePopupMenu();
        for (flags, id, text) in items {
            AppendMenuW(menu, MF_STRING | flags, id, wide(&text).as_ptr());
        }
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd); // required so the menu closes when clicking elsewhere
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        DestroyMenu(menu);
        match cmd {
            10 => drop(s.tx.send(TrayCmd::ToggleService)),
            1 => open_console(s),
            2 => drop(s.tx.send(TrayCmd::Mode(TrackpadMode::Mouse))),
            3 => drop(s.tx.send(TrayCmd::Mode(TrackpadMode::Direction))),
            4 => drop(s.tx.send(TrayCmd::Mode(TrackpadMode::Idle))),
            5 => set_autostart(!autostart_enabled()),
            6 => drop(Command::new("notepad").arg(log_path()).spawn()),
            9 => drop(s.tx.send(TrayCmd::Quit)),
            _ => {}
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TRAY {
        if let Some(s) = STATE.get() {
            match lp as u32 {
                WM_LBUTTONDBLCLK => {
                    if s.running.load(Ordering::SeqCst) {
                        open_console(s);
                    } else {
                        show_menu(hwnd, s);
                    }
                }
                WM_RBUTTONUP | WM_LBUTTONUP => show_menu(hwnd, s),
                _ => {}
            }
        }
        return 0;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Start the tray icon on its own thread. Menu actions arrive on the returned receiver.
pub fn spawn(
    title: String,
    url: Option<String>,
    mode: Arc<AtomicU8>,
    running: Arc<std::sync::atomic::AtomicBool>,
) -> UnboundedReceiver<TrayCmd> {
    let (tx, rx) = unbounded_channel();
    let _ = STATE.set(State {
        tx,
        mode,
        url,
        title,
        running,
    });
    std::thread::spawn(|| unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("AtvTrayWindow");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );

        let png = include_bytes!("tray_icon.png");
        let size = GetSystemMetrics(SM_CXSMICON);
        let mut icon = CreateIconFromResourceEx(png.as_ptr(), png.len() as u32, 1, 0x30000, size, size, 0);
        if icon.is_null() {
            icon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        }

        let s = STATE.get().unwrap();
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_INFO;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        fill(&mut nid.szTip, &s.title);
        fill(&mut nid.szInfoTitle, "Apple TV Remote 已在后台运行");
        fill(&mut nid.szInfo, &format!("{}\n右键或点击托盘图标可开启/停止服务或切换模式", s.title));
        Shell_NotifyIconW(NIM_ADD, &nid);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Shell_NotifyIconW(NIM_DELETE, &nid);
    });
    rx
}

/// Update the tray icon tooltip and status balloon when service starts or stops.
pub fn update_status(running: bool, info: &str) {
    unsafe {
        let class = wide("AtvTrayWindow");
        let hwnd = FindWindowExW(HWND_MESSAGE, std::ptr::null_mut(), class.as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_TIP | NIF_INFO;
            if running {
                fill(&mut nid.szTip, &format!("Apple TV Remote (运行中) · {info}"));
                fill(&mut nid.szInfoTitle, "服务已启动");
                fill(&mut nid.szInfo, &format!("Apple TV Remote 服务已恢复运行\n{info}"));
            } else {
                fill(&mut nid.szTip, "Apple TV Remote (已停止服务)");
                fill(&mut nid.szInfoTitle, "服务已停止");
                fill(&mut nid.szInfo, "Apple TV Remote 模拟服务已暂停，右键可重新启动。");
            }
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// Remove the tray icon (called on shutdown so no ghost icon remains).
pub fn remove() {
    unsafe {
        let class = wide("AtvTrayWindow");
        let hwnd = FindWindowExW(HWND_MESSAGE, std::ptr::null_mut(), class.as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

//! 托盘图标、右键菜单，以及运行时绘制的程序图标（不依赖资源文件）。

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

pub const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;

fn fill_utf16(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    for (d, u) in dst.iter_mut().zip(s.encode_utf16().take(max).chain(std::iter::repeat(0))) {
        *d = u;
    }
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: TRAY_ID, ..Default::default() }
}

pub fn add(hwnd: HWND, icon: HICON, tip: &str) {
    let mut d = base(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon;
    fill_utf16(&mut d.szTip, tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
    }
}

pub fn set_tip(hwnd: HWND, tip: &str) {
    let mut d = base(hwnd);
    d.uFlags = NIF_TIP;
    fill_utf16(&mut d.szTip, tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

/// 托盘气泡通知。
pub fn notify(hwnd: HWND, title: &str, text: &str) {
    let mut d = base(hwnd);
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = NIIF_INFO;
    fill_utf16(&mut d.szInfoTitle, title);
    fill_utf16(&mut d.szInfo, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

pub fn remove(hwnd: HWND) {
    let d = base(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &d);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCmd {
    ShowPanel = 1,
    TogglePause,
    ToggleAutostart,
    OpenConfig,
    ReloadConfig,
    OpenDataDir,
    ClearHistory,
    Exit,
}

const ALL_CMDS: [MenuCmd; 8] = [
    MenuCmd::ShowPanel,
    MenuCmd::TogglePause,
    MenuCmd::ToggleAutostart,
    MenuCmd::OpenConfig,
    MenuCmd::ReloadConfig,
    MenuCmd::OpenDataDir,
    MenuCmd::ClearHistory,
    MenuCmd::Exit,
];

pub struct MenuState {
    pub panel_hotkey: String,
    pub paused: bool,
    pub autostart: bool,
}

/// 弹出右键菜单并返回选中的命令。菜单是模态循环，调用时不要持有应用状态的借用。
pub fn show_menu(hwnd: HWND, st: &MenuState) -> Option<MenuCmd> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let open_label = if st.panel_hotkey.is_empty() {
            "打开历史".to_string()
        } else {
            format!("打开历史\t{}", st.panel_hotkey)
        };
        let items: [(MENU_ITEM_FLAGS, MenuCmd, &str); 8] = [
            (MF_STRING | MF_DEFAULT, MenuCmd::ShowPanel, &open_label),
            (MF_STRING | check(st.paused), MenuCmd::TogglePause, "暂停记录"),
            (MF_STRING | check(st.autostart), MenuCmd::ToggleAutostart, "开机自启"),
            (MF_STRING, MenuCmd::OpenConfig, "编辑配置文件"),
            (MF_STRING, MenuCmd::ReloadConfig, "重新加载配置"),
            (MF_STRING, MenuCmd::OpenDataDir, "打开数据目录"),
            (MF_STRING, MenuCmd::ClearHistory, "清空历史…"),
            (MF_STRING, MenuCmd::Exit, "退出"),
        ];
        for (i, (flags, cmd, label)) in items.iter().enumerate() {
            if i == 3 || i == 6 || i == 7 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            }
            let _ = AppendMenuW(menu, *flags, *cmd as usize, &HSTRING::from(*label));
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 不先激活自己的窗口，点菜单外部时菜单不会消失
        let _ = SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        ALL_CMDS.into_iter().find(|c| *c as i32 == id.0)
    }
}

/// 画一个 n×n 的剪贴板图标：蓝色板子、深色夹子、白色横线。
pub fn create_icon(n: i32) -> HICON {
    let n = n.max(16);
    let mut pixels = vec![0u32; (n * n) as usize];
    let inside = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32| x >= x0 && x < x1 && y >= y0 && y < y1;
    for py in 0..n {
        for px in 0..n {
            let x = (px as f32 + 0.5) / n as f32;
            let y = (py as f32 + 0.5) / n as f32;
            let mut c = 0u32;
            if inside(x, y, 0.14, 0.12, 0.86, 0.96) {
                // 切掉四个角，近似圆角
                let corner = !(0.2..=0.8).contains(&x) && !(0.18..=0.9).contains(&y);
                if !corner {
                    c = 0xFF25_63EB;
                }
            }
            let line = |y0: f32, x1: f32| inside(x, y, 0.28, y0, x1, y0 + 0.08);
            if line(0.40, 0.72) || line(0.56, 0.72) || line(0.72, 0.58) {
                c = 0xFFFF_FFFF;
            }
            if inside(x, y, 0.34, 0.04, 0.66, 0.24) {
                c = 0xFF1E_293B;
            }
            pixels[(py * n + px) as usize] = c;
        }
    }
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        };
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());
        let mask_bits = vec![0u8; (((n + 15) / 16) * 2 * n) as usize];
        let mask = CreateBitmap(n, n, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// 用记事本打开文件，或用资源管理器打开目录。
pub fn open_path(path: &std::path::Path, with_notepad: bool) {
    let target = HSTRING::from(path.as_os_str());
    unsafe {
        if with_notepad {
            ShellExecuteW(None, w!("open"), w!("notepad.exe"), &target, None, SW_SHOWNORMAL);
        } else {
            ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL);
        }
    }
}

//! 历史面板：原生 Win32 弹窗（搜索框 + 列表）。窗口提前创建，平时隐藏，唤出很快。
//!
//! 面板只负责展示和选择，选中后把 key 投递给宿主窗口（PostMessage），由宿主执行粘贴，
//! 这样面板和应用状态之间不会发生重入借用。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::input::key_down;

/// wparam = 条目 key，lparam = 1 表示纯文本粘贴。
pub const WM_PANEL_PASTE: u32 = WM_APP + 10;
/// wparam = 条目 key。
pub const WM_PANEL_DELETE: u32 = WM_APP + 11;
/// 搜索框内容变了；宿主用 query() 读取后搜索，再用 set_rows() 回填结果。
pub const WM_PANEL_QUERY: u32 = WM_APP + 12;

/// 列表最多显示多少条。
pub const MAX_ROWS: usize = 200;

const CLASS: PCWSTR = w!("clipstack_panel");
const ID_EDIT: i32 = 100;
const ID_LIST: i32 = 101;
const EM_SETCUEBANNER: u32 = 0x1501;
const CUE: PCWSTR = w!("搜索…   Enter 粘贴 · Shift+Enter 纯文本 · Ctrl+1~9 快速粘贴 · Shift+Del 删除 · Esc 关闭");

pub struct Row {
    pub key: i64,
    pub label: String,
}

struct Panel {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    host: HWND,
    font: HFONT,
    dpi: u32,
    /// 列表里正在显示的行。
    rows: Vec<Row>,
}

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
    static EDIT_PROC: Cell<isize> = const { Cell::new(0) };
}

fn with_panel<R>(f: impl FnOnce(&mut Panel) -> R) -> Option<R> {
    PANEL.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn create(host: HWND, icon: HICON) -> windows::core::Result<()> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(panel_proc),
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("clipstack"),
            WS_POPUP | WS_BORDER,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(HMENU(ID_EDIT as usize as *mut c_void)),
            Some(hinst),
            None,
        )?;
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE((LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_USETABSTOPS) as u32),
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(HMENU(ID_LIST as usize as *mut c_void)),
            Some(hinst),
            None,
        )?;
        SendMessageW(edit, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(CUE.as_ptr() as isize)));
        // 子类化搜索框，拦截上下键、回车等
        let orig = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        EDIT_PROC.set(orig);

        PANEL.with(|p| {
            *p.borrow_mut() = Some(Panel { hwnd, edit, list, host, font: HFONT::default(), dpi: 0, rows: Vec::new() })
        });
        update_dpi(GetDpiForWindow(hwnd));
        Ok(())
    }
}

fn make_font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            0,
            dpi,
        );
        if ok.is_err() {
            return HFONT::default();
        }
        let mut lf = ncm.lfMessageFont;
        lf.lfHeight = lf.lfHeight * 115 / 100;
        CreateFontIndirectW(&lf)
    }
}

fn update_dpi(dpi: u32) {
    let Some(Some((edit, list, old))) = with_panel(|p| {
        if p.dpi == dpi {
            return None;
        }
        let old = p.font;
        p.font = make_font(dpi);
        p.dpi = dpi;
        Some((p.edit, p.list, (old, p.font)))
    }) else {
        return;
    };
    let (old_font, new_font) = old;
    unsafe {
        for h in [edit, list] {
            SendMessageW(h, WM_SETFONT, Some(WPARAM(new_font.0 as usize)), Some(LPARAM(1)));
        }
        if !old_font.is_invalid() {
            let _ = DeleteObject(old_font.into());
        }
    }
    layout();
}

fn layout() {
    let Some((hwnd, edit, list, dpi)) = with_panel(|p| (p.hwnd, p.edit, p.list, p.dpi)) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let pad = scale(8, dpi);
        let edit_h = scale(30, dpi);
        let width = rc.right - 2 * pad;
        let _ = MoveWindow(edit, pad, pad, width, edit_h, true);
        let _ = MoveWindow(list, pad, 2 * pad + edit_h, width, rc.bottom - 3 * pad - edit_h, true);
    }
}

pub fn is_visible() -> bool {
    with_panel(|p| p.hwnd).is_some_and(|h| unsafe { IsWindowVisible(h) }.as_bool())
}

pub fn hide() {
    if let Some(hwnd) = with_panel(|p| p.hwnd) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// 在鼠标附近显示面板，搜索框清空，列表显示 `rows`（最近的记录）。
pub fn show(rows: Vec<Row>) {
    let Some((hwnd, edit)) = with_panel(|p| (p.hwnd, p.edit)) else {
        return;
    };
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let (mut dpi, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let (w, h) = (scale(600, dpi), scale(460, dpi));
        let wa = mi.rcWork;
        let x = pt.x.min(wa.right - w).max(wa.left);
        let y = pt.y.min(wa.bottom - h).max(wa.top);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        update_dpi(GetDpiForWindow(hwnd));
        layout();
        let _ = SetWindowTextW(edit, w!(""));
    }
    set_rows(rows);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        force_foreground(hwnd);
        let _ = SetFocus(Some(edit));
    }
}

/// 后台程序抢前台焦点：先正常尝试，不行就临时挂接到前台线程的输入队列。
unsafe fn force_foreground(hwnd: HWND) {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, true);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

/// 搜索框当前内容。
pub fn query() -> String {
    with_panel(|p| p.edit).map(window_text).unwrap_or_default()
}

/// 替换列表内容并选中第一行。
pub fn set_rows(mut rows: Vec<Row>) {
    rows.truncate(MAX_ROWS);
    let Some((list, labels)) = with_panel(|p| {
        p.rows = rows;
        let labels: Vec<Vec<u16>> =
            p.rows.iter().enumerate().map(|(n, r)| wide(&format!("{}\t{}", n + 1, r.label))).collect();
        (p.list, labels)
    }) else {
        return;
    };
    render(list, &labels);
}

fn render(list: HWND, labels: &[Vec<u16>]) {
    unsafe {
        SendMessageW(list, WM_SETREDRAW, Some(WPARAM(0)), None);
        SendMessageW(list, LB_RESETCONTENT, None, None);
        for label in labels {
            SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(label.as_ptr() as isize)));
        }
        SendMessageW(list, LB_SETCURSEL, Some(WPARAM(0)), None);
        SendMessageW(list, WM_SETREDRAW, Some(WPARAM(1)), None);
        let _ = InvalidateRect(Some(list), None, true);
    }
}

fn move_selection(delta: isize) {
    let Some(list) = with_panel(|p| p.list) else {
        return;
    };
    unsafe {
        let count = SendMessageW(list, LB_GETCOUNT, None, None).0;
        if count <= 0 {
            return;
        }
        let cur = SendMessageW(list, LB_GETCURSEL, None, None).0.max(0);
        let next = (cur + delta).clamp(0, count - 1);
        SendMessageW(list, LB_SETCURSEL, Some(WPARAM(next as usize)), None);
    }
}

fn selected_index(list: HWND, index: Option<usize>) -> Option<usize> {
    match index {
        Some(i) => Some(i),
        None => usize::try_from(unsafe { SendMessageW(list, LB_GETCURSEL, None, None) }.0).ok(),
    }
}

/// 粘贴选中项（或第 index 项）。
fn activate(index: Option<usize>, plain: bool) {
    let Some((hwnd, list, host)) = with_panel(|p| (p.hwnd, p.list, p.host)) else {
        return;
    };
    let Some(sel) = selected_index(list, index) else {
        return;
    };
    let Some(Some(key)) = with_panel(|p| p.rows.get(sel).map(|r| r.key)) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = PostMessageW(Some(host), WM_PANEL_PASTE, WPARAM(key as usize), LPARAM(plain as isize));
    }
}

fn delete_selected() {
    let Some((list, host)) = with_panel(|p| (p.list, p.host)) else {
        return;
    };
    let Some(sel) = selected_index(list, None) else {
        return;
    };
    let Some(Some((key, rows))) = with_panel(|p| {
        (sel < p.rows.len()).then(|| {
            let mut rows = std::mem::take(&mut p.rows);
            let key = rows.remove(sel).key;
            (key, rows)
        })
    }) else {
        return;
    };
    unsafe {
        let _ = PostMessageW(Some(host), WM_PANEL_DELETE, WPARAM(key as usize), LPARAM(0));
    }
    set_rows(rows);
    unsafe {
        let count = SendMessageW(list, LB_GETCOUNT, None, None).0;
        if count > 0 {
            let next = (sel as isize).min(count - 1);
            SendMessageW(list, LB_SETCURSEL, Some(WPARAM(next as usize)), None);
        }
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            let ctrl = key_down(VK_CONTROL);
            let shift = key_down(VK_SHIFT);
            let vk = VIRTUAL_KEY(wp.0 as u16);
            let handled = match vk {
                VK_DOWN => {
                    move_selection(1);
                    true
                }
                VK_UP => {
                    move_selection(-1);
                    true
                }
                VK_NEXT => {
                    move_selection(10);
                    true
                }
                VK_PRIOR => {
                    move_selection(-10);
                    true
                }
                VK_RETURN => {
                    activate(None, shift);
                    true
                }
                VK_ESCAPE => {
                    hide();
                    true
                }
                VK_DELETE if shift => {
                    delete_selected();
                    true
                }
                _ if ctrl && (b'1' as u16..=b'9' as u16).contains(&vk.0) => {
                    activate(Some((vk.0 - b'1' as u16) as usize), false);
                    true
                }
                _ => false,
            };
            if handled {
                return LRESULT(0);
            }
        }
        // 吞掉回车 / Esc 产生的字符，否则单行编辑框会“叮”一声
        WM_CHAR if matches!(wp.0, 0x0A | 0x0D | 0x1B) => return LRESULT(0),
        _ => {}
    }
    unsafe {
        let orig: WNDPROC = std::mem::transmute(EDIT_PROC.get());
        CallWindowProcW(orig, hwnd, msg, wp, lp)
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout();
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            match (id, code) {
                (ID_EDIT, EN_CHANGE) => {
                    // 交给宿主搜索；连续输入时多条消息会在宿主那边按内容去重
                    if let Some(host) = with_panel(|p| p.host) {
                        let _ = unsafe { PostMessageW(Some(host), WM_PANEL_QUERY, WPARAM(0), LPARAM(0)) };
                    }
                }
                (ID_LIST, LBN_SELCHANGE) => {
                    if let Some(edit) = with_panel(|p| p.edit) {
                        let _ = unsafe { SetFocus(Some(edit)) };
                    }
                }
                (ID_LIST, LBN_DBLCLK) => activate(None, false),
                _ => {}
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_CLOSE => {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

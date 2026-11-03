//! 宿主窗口与应用状态：监听剪贴板、处理热键、托盘菜单、粘贴和宏。
//!
//! 一切都跑在主线程的消息循环里；SQLite 写入交给 store 的后台线程。
//! 状态放在 thread_local 的 RefCell 中，弹出模态循环（菜单、消息框）前必须先释放借用。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::clip::{self, ClipData, Formats, ReadOptions};
use crate::config::{Config, Macro, Step};
use crate::history::{Entry, History};
use crate::keys::Combo;
use crate::panel::{self, Row};
use crate::store::{Record, Store};
use crate::tray::{self, MenuCmd, MenuState};
use crate::{autostart, input, log};

pub const HOST_CLASS: PCWSTR = w!("clipstack_host");
/// 第二个实例启动时发给已运行实例：打开历史面板。
pub const WM_SHOW_PANEL: u32 = WM_APP + 20;

const TIMER_RESTORE: usize = 1;
const HK_PASTE_PREVIOUS: i32 = 1;
const HK_SHOW_PANEL: i32 = 2;
const HK_TOGGLE_PAUSE: i32 = 3;
const HK_MACRO_BASE: i32 = 100;
const MODIFIER_TIMEOUT: Duration = Duration::from_millis(1500);

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug)]
enum Action {
    PastePrevious,
    ShowPanel,
    TogglePause,
    Macro(usize),
}

pub struct Init {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub data_dir: PathBuf,
    pub store: Store,
    pub entries: Vec<Entry>,
    pub first_run: bool,
    pub startup_errors: Vec<String>,
}

struct App {
    hwnd: HWND,
    icon: HICON,
    cfg: Config,
    macros: Vec<Macro>,
    cfg_path: PathBuf,
    data_dir: PathBuf,
    store: Store,
    history: History,
    formats: Formats,
    paused: bool,
    /// 我们自己最后一次写剪贴板后的序列号，对应的更新通知要忽略。
    own_seq: u32,
    last_seq: u32,
    /// 当前剪贴板内容是否就是 history[0]（被忽略的内容不算），决定粘贴后要不要恢复。
    current_recorded: bool,
    prev_foreground: HWND,
    hotkeys: HashMap<i32, Action>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn beep() {
    let _ = unsafe { MessageBeep(MB_OK) };
}

pub fn message_box(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("clipstack"), MB_OK | MB_ICONWARNING | MB_SETFOREGROUND);
    }
}

fn confirm(text: &str) -> bool {
    let res = unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("clipstack"), MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND)
    };
    res == IDYES
}

/// 切回目标窗口，等它真正拿到前台再发按键。
fn focus_window(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let start = Instant::now();
        while GetForegroundWindow() != hwnd && start.elapsed() < Duration::from_millis(300) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    thread::sleep(Duration::from_millis(30));
}

pub fn run(init: Init) {
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else {
            return;
        };
        let hinst: HINSTANCE = hinst.into();
        let icon = tray::create_icon(GetSystemMetrics(SM_CXSMICON));
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_proc),
            hInstance: hinst,
            hIcon: icon,
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW,
            HOST_CLASS,
            w!("clipstack"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                message_box(&format!("创建窗口失败：{e}"));
                return;
            }
        };
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        if let Err(e) = panel::create(hwnd, icon) {
            log::write(format!("panel create failed: {e}"));
        }
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            message_box(&format!("无法监听剪贴板：{e}"));
        }
        tray::add(hwnd, icon, "clipstack");

        let macros = init.cfg.compile_macros().unwrap_or_default();
        let max_items = init.cfg.general.max_items;
        let mut app = App {
            hwnd,
            icon,
            cfg: init.cfg,
            macros,
            cfg_path: init.cfg_path,
            data_dir: init.data_dir,
            store: init.store,
            history: History::new(init.entries, max_items),
            formats: Formats::register(),
            paused: false,
            own_seq: 0,
            last_seq: 0,
            current_recorded: false,
            prev_foreground: HWND::default(),
            hotkeys: HashMap::new(),
        };
        let mut errors = init.startup_errors;
        errors.extend(app.register_hotkeys());
        // 把启动时剪贴板里已有的内容记为“当前”
        app.on_clipboard_update();
        let panel_hotkey = app.cfg.hotkeys.show_panel.clone();
        let paste_hotkey = app.cfg.hotkeys.paste_previous.clone();
        APP.with(|a| *a.borrow_mut() = Some(app));

        if init.first_run {
            tray::notify(
                hwnd,
                "clipstack 已在后台运行",
                &format!("{panel_hotkey} 打开剪贴板历史，{paste_hotkey} 粘贴上一条。右键托盘图标可修改设置。"),
            );
        }
        if !errors.is_empty() {
            message_box(&errors.join("\n"));
        }
    };

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 释放 App：Store 的 Drop 会等后台线程把剩余写入做完
    drop(APP.with(|a| a.borrow_mut().take()));
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            with_app(|a| a.on_clipboard_update());
        }
        WM_HOTKEY => {
            with_app(|a| a.on_hotkey(wp.0 as i32));
        }
        WM_TIMER if wp.0 == TIMER_RESTORE => {
            with_app(|a| a.restore_current());
        }
        tray::WM_TRAY => match (lp.0 & 0xFFFF) as u32 {
            WM_LBUTTONUP => {
                with_app(|a| a.show_panel());
            }
            WM_RBUTTONUP => tray_menu(hwnd),
            _ => {}
        },
        panel::WM_PANEL_PASTE => {
            with_app(|a| a.paste_key(wp.0 as i64, lp.0 != 0));
        }
        panel::WM_PANEL_DELETE => {
            with_app(|a| a.delete_key(wp.0 as i64));
        }
        WM_SHOW_PANEL => {
            with_app(|a| a.show_panel());
        }
        WM_ENDSESSION if wp.0 != 0 => {
            // 注销 / 关机：立即落盘
            with_app(|a| a.cleanup());
            drop(APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.take())));
        }
        WM_DESTROY => {
            with_app(|a| a.cleanup());
            unsafe { PostQuitMessage(0) };
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // 资源管理器重启后托盘图标会丢，需要重新添加
            with_app(|a| tray::add(a.hwnd, a.icon, &a.tip()));
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn tray_menu(hwnd: HWND) {
    let Some(state) = with_app(|a| a.menu_state()) else {
        return;
    };
    let Some(cmd) = tray::show_menu(hwnd, &state) else {
        return;
    };
    match cmd {
        MenuCmd::ClearHistory => {
            if confirm("确定清空全部剪贴板历史吗？此操作不能撤销。") {
                with_app(|a| a.clear_history());
            }
        }
        MenuCmd::Exit => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {
            if let Some(Some(msg)) = with_app(|a| a.on_menu(cmd)) {
                message_box(&msg);
            }
        }
    }
}

impl App {
    fn tip(&self) -> String {
        if self.paused { "clipstack（已暂停记录）".into() } else { "clipstack".into() }
    }

    fn menu_state(&self) -> MenuState {
        MenuState {
            panel_hotkey: self.cfg.hotkeys.show_panel.clone(),
            paused: self.paused,
            autostart: autostart::is_enabled(),
        }
    }

    fn on_menu(&mut self, cmd: MenuCmd) -> Option<String> {
        match cmd {
            MenuCmd::ShowPanel => self.show_panel(),
            MenuCmd::TogglePause => self.toggle_pause(),
            MenuCmd::ToggleAutostart => {
                if let Err(e) = autostart::set(!autostart::is_enabled()) {
                    return Some(e);
                }
            }
            MenuCmd::OpenConfig => tray::open_path(&self.cfg_path, true),
            MenuCmd::ReloadConfig => return self.reload_config(),
            MenuCmd::OpenDataDir => tray::open_path(&self.data_dir, false),
            MenuCmd::ClearHistory | MenuCmd::Exit => {}
        }
        None
    }

    fn register_hotkeys(&mut self) -> Vec<String> {
        for id in self.hotkeys.keys() {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), *id) };
        }
        self.hotkeys.clear();
        let hk = &self.cfg.hotkeys;
        let mut wanted = vec![
            (HK_PASTE_PREVIOUS, hk.paste_previous.clone(), Action::PastePrevious, "粘贴上一条".to_string()),
            (HK_SHOW_PANEL, hk.show_panel.clone(), Action::ShowPanel, "打开历史".to_string()),
            (HK_TOGGLE_PAUSE, hk.toggle_pause.clone(), Action::TogglePause, "暂停记录".to_string()),
        ];
        for (i, m) in self.macros.iter().enumerate() {
            wanted.push((HK_MACRO_BASE + i as i32, m.hotkey.clone(), Action::Macro(i), format!("宏 {:?}", m.name)));
        }
        let mut errors = Vec::new();
        for (id, text, action, label) in wanted {
            if text.trim().is_empty() {
                continue;
            }
            match Combo::parse(&text) {
                Err(e) => errors.push(format!("{label}：{e}")),
                Ok(c) => match unsafe { RegisterHotKey(Some(self.hwnd), id, c.hotkey_modifiers(), c.vk as u32) } {
                    Ok(()) => {
                        self.hotkeys.insert(id, action);
                    }
                    Err(_) => errors.push(format!("{label} 的快捷键 {text} 注册失败，可能已被其他程序占用")),
                },
            }
        }
        errors
    }

    fn reload_config(&mut self) -> Option<String> {
        let cfg = match Config::load(&self.cfg_path) {
            Ok(c) => c,
            Err(e) => return Some(format!("配置文件有误，未生效：\n{e}")),
        };
        self.macros = cfg.compile_macros().unwrap_or_default();
        self.history.set_max(cfg.general.max_items);
        self.store.set_max(cfg.general.max_items);
        self.cfg = cfg;
        let errors = self.register_hotkeys();
        if errors.is_empty() {
            tray::notify(self.hwnd, "clipstack", "配置已重新加载");
            None
        } else {
            Some(errors.join("\n"))
        }
    }

    fn cleanup(&mut self) {
        unsafe {
            for id in self.hotkeys.keys() {
                let _ = UnregisterHotKey(Some(self.hwnd), *id);
            }
            let _ = RemoveClipboardFormatListener(self.hwnd);
        }
        self.hotkeys.clear();
        tray::remove(self.hwnd);
    }

    fn on_hotkey(&mut self, id: i32) {
        match self.hotkeys.get(&id).copied() {
            Some(Action::PastePrevious) => self.paste_previous(),
            Some(Action::ShowPanel) => {
                if panel::is_visible() {
                    panel::hide();
                } else {
                    self.show_panel();
                }
            }
            Some(Action::TogglePause) => self.toggle_pause(),
            Some(Action::Macro(i)) => self.run_macro(i),
            None => {}
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        tray::set_tip(self.hwnd, &self.tip());
        tray::notify(self.hwnd, "clipstack", if self.paused { "已暂停记录" } else { "已恢复记录" });
    }

    fn on_clipboard_update(&mut self) {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == self.own_seq || seq == self.last_seq {
            return;
        }
        self.last_seq = seq;
        self.current_recorded = false;
        if self.paused {
            return;
        }
        if let Some(exe) = clip::owner_exe()
            && self.cfg.general.ignore_apps.iter().any(|a| a.eq_ignore_ascii_case(&exe))
        {
            return;
        }
        let opts = ReadOptions { images: self.cfg.general.record_images, files: self.cfg.general.record_files };
        let Some(data) = clip::read(self.hwnd, &self.formats, &opts) else {
            return;
        };
        if data.size() > self.cfg.max_item_bytes() {
            return;
        }
        self.record(data);
    }

    fn record(&mut self, data: ClipData) {
        let entry = Entry {
            key: data.key(),
            kind: data.kind(),
            preview: data.preview(),
            search: data.search_text(),
            last_used: now_ms(),
        };
        self.history.push_front(entry.clone());
        self.store.upsert(Record { entry, data });
        self.current_recorded = true;
    }

    fn write_clip(&mut self, data: &ClipData, plain: bool) -> bool {
        let ok = clip::write(self.hwnd, &self.formats, data, plain);
        self.own_seq = unsafe { GetClipboardSequenceNumber() };
        ok
    }

    /// 把第 `index` 条（从 0 开始）写进剪贴板。
    fn put_history(&mut self, index: usize, plain: bool) -> bool {
        let Some(key) = self.history.get(index).map(|e| e.key) else {
            return false;
        };
        match self.store.load(key) {
            Some(data) => self.write_clip(&data, plain),
            None => false,
        }
    }

    fn cancel_restore(&self) {
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_RESTORE) };
    }

    fn schedule_restore(&self) {
        unsafe { SetTimer(Some(self.hwnd), TIMER_RESTORE, self.cfg.general.restore_delay_ms.max(50), None) };
    }

    /// 把剪贴板恢复成当前内容（history[0]）。
    fn restore_current(&mut self) {
        self.cancel_restore();
        if self.put_history(0, false) {
            self.current_recorded = true;
        }
    }

    /// Ctrl+Shift+V：粘贴上一条，之后把剪贴板恢复成当前内容，Ctrl+V 不受影响。
    fn paste_previous(&mut self) {
        self.cancel_restore();
        let restore = self.current_recorded;
        if !self.put_history(1, false) {
            beep();
            return;
        }
        input::wait_modifiers_released(MODIFIER_TIMEOUT);
        input::send_combo(&Combo::CTRL_V);
        if restore {
            self.schedule_restore();
        }
    }

    fn show_panel(&mut self) {
        self.prev_foreground = unsafe { GetForegroundWindow() };
        let rows = self
            .history
            .iter()
            .map(|e| Row {
                key: e.key,
                label: if e.preview.is_empty() { "（空白）".into() } else { e.preview.clone() },
                search: e.search.clone(),
            })
            .collect();
        panel::show(rows);
    }

    /// 面板里选中某条：它变成当前内容并粘贴到之前的窗口。
    fn paste_key(&mut self, key: i64, plain: bool) {
        self.cancel_restore();
        let Some(data) = self.store.load(key) else {
            beep();
            return;
        };
        if !self.write_clip(&data, plain) {
            beep();
            return;
        }
        let now = now_ms();
        if self.history.touch(key, now) {
            self.store.touch(key, now);
        }
        self.current_recorded = true;
        focus_window(self.prev_foreground);
        input::wait_modifiers_released(MODIFIER_TIMEOUT);
        input::send_combo(&Combo::CTRL_V);
    }

    fn delete_key(&mut self, key: i64) {
        if self.history.remove(key) {
            self.store.delete(key);
        }
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.store.clear();
        self.current_recorded = false;
    }

    fn run_macro(&mut self, i: usize) {
        let Some(m) = self.macros.get(i).cloned() else {
            return;
        };
        self.cancel_restore();
        let restore = self.current_recorded;
        let mut used_clipboard = false;
        input::wait_modifiers_released(MODIFIER_TIMEOUT);
        for step in &m.steps {
            match step {
                Step::Paste { index, plain } => {
                    if !self.put_history(index - 1, *plain) {
                        log::write(format!("macro {:?}: no history item #{index}", m.name));
                        beep();
                        break;
                    }
                    used_clipboard = true;
                    input::send_combo(&Combo::CTRL_V);
                    thread::sleep(Duration::from_millis(self.cfg.general.macro_paste_delay_ms));
                }
                Step::Key(c) => {
                    input::send_combo(c);
                    thread::sleep(Duration::from_millis(20));
                }
                Step::Text(t) => {
                    input::type_text(t);
                    thread::sleep(Duration::from_millis(20));
                }
                Step::Sleep(ms) => thread::sleep(Duration::from_millis(*ms)),
            }
        }
        if used_clipboard && restore {
            self.schedule_restore();
        }
    }
}

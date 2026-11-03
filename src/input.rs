//! 用 SendInput 模拟键盘：发送组合键、输入文字、等待用户松开修饰键。

use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::keys::Combo;

/// 同时检查不分左右的 VK_CONTROL 等：有些程序（自动化工具、远程桌面）注入的是这种键，
/// 只设置通用键状态，不设置 L/R 键状态。
const MODIFIERS: [VIRTUAL_KEY; 11] = [
    VK_CONTROL,
    VK_SHIFT,
    VK_MENU,
    VK_LCONTROL,
    VK_RCONTROL,
    VK_LSHIFT,
    VK_RSHIFT,
    VK_LMENU,
    VK_RMENU,
    VK_LWIN,
    VK_RWIN,
];

/// 一个没有分配功能的虚拟键，用来防止单独松开 Alt / Win 时弹出菜单或开始菜单。
const VK_UNASSIGNED: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

fn is_down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(vk.0 as i32) }) < 0
}

pub fn key_down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetKeyState(vk.0 as i32) }) < 0
}

fn is_extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_UP
            | VK_DOWN
            | VK_LEFT
            | VK_RIGHT
            | VK_APPS
            | VK_DIVIDE
            | VK_NUMLOCK
    )
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn unicode(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: unit, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if !inputs.is_empty() {
        unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    }
}

/// 等用户松开 Ctrl / Shift / Alt / Win，避免和我们模拟的按键叠加
/// （例如按着 Shift 时发 Ctrl+V，会变成 Ctrl+Shift+V）。超时后强制松开。
pub fn wait_modifiers_released(timeout: Duration) {
    let start = Instant::now();
    let mut waited = false;
    while MODIFIERS.iter().any(|&vk| is_down(vk)) {
        if start.elapsed() >= timeout {
            release_held_modifiers();
            break;
        }
        waited = true;
        thread::sleep(Duration::from_millis(10));
    }
    if waited {
        // 给目标程序一点时间处理完松键消息
        thread::sleep(Duration::from_millis(30));
    }
}

fn release_held_modifiers() {
    let held: Vec<VIRTUAL_KEY> = MODIFIERS.iter().copied().filter(|&vk| is_down(vk)).collect();
    if held.is_empty() {
        return;
    }
    let mut inputs = vec![key(VK_UNASSIGNED, false), key(VK_UNASSIGNED, true)];
    inputs.extend(held.into_iter().map(|vk| key(vk, true)));
    send(&inputs);
}

pub fn send_combo(c: &Combo) {
    let mods: Vec<VIRTUAL_KEY> = [(c.ctrl, VK_LCONTROL), (c.shift, VK_LSHIFT), (c.alt, VK_LMENU), (c.win, VK_LWIN)]
        .into_iter()
        .filter_map(|(on, vk)| on.then_some(vk))
        .collect();
    let main = VIRTUAL_KEY(c.vk);
    let mut inputs: Vec<INPUT> = mods.iter().map(|&vk| key(vk, false)).collect();
    inputs.push(key(main, false));
    inputs.push(key(main, true));
    if c.alt || c.win {
        inputs.push(key(VK_UNASSIGNED, false));
        inputs.push(key(VK_UNASSIGNED, true));
    }
    inputs.extend(mods.iter().rev().map(|&vk| key(vk, true)));
    send(&inputs);
}

/// 直接输入文字（不经过剪贴板）。换行按 Enter，制表符按 Tab。
pub fn type_text(text: &str) {
    let mut inputs = Vec::new();
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => inputs.extend([key(VK_RETURN, false), key(VK_RETURN, true)]),
            '\t' => inputs.extend([key(VK_TAB, false), key(VK_TAB, true)]),
            _ => {
                let mut buf = [0u16; 2];
                for &unit in ch.encode_utf16(&mut buf).iter() {
                    inputs.extend([unicode(unit, false), unicode(unit, true)]);
                }
            }
        }
    }
    send(&inputs);
}

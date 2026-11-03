#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod clip;
mod config;
mod history;
mod input;
mod keys;
mod log;
mod panel;
mod store;
mod tray;

use std::path::PathBuf;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
use windows::core::{PCWSTR, w};

use crate::config::Config;
use crate::store::Store;

/// 返回 (配置目录, 数据目录, 是否便携模式)。
/// 设置环境变量 CLIPSTACK_HOME 进入便携模式：配置和数据都放在该目录，首次运行也不写开机自启。
fn dirs() -> (PathBuf, PathBuf, bool) {
    if let Some(home) = std::env::var_os("CLIPSTACK_HOME") {
        let p = PathBuf::from(home);
        return (p.clone(), p, true);
    }
    let env = |k: &str| std::env::var_os(k).map_or_else(|| PathBuf::from("."), PathBuf::from);
    (env("APPDATA").join("clipstack"), env("LOCALAPPDATA").join("clipstack"), false)
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 单实例：已经在运行就让它打开历史面板
    let _instance = unsafe { CreateMutexW(None, false, w!("Local\\clipstack-single-instance")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            if let Ok(hwnd) = FindWindowW(app::HOST_CLASS, PCWSTR::null()) {
                let _ = PostMessageW(Some(hwnd), app::WM_SHOW_PANEL, WPARAM(0), LPARAM(0));
            }
        }
        return;
    }

    let (config_dir, data_dir, portable) = dirs();
    for dir in [&config_dir, &data_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            app::message_box(&format!("无法创建目录 {}：{e}", dir.display()));
            return;
        }
    }
    log::init(data_dir.join("clipstack.log"));

    let cfg_path = config_dir.join("config.toml");
    let first_run = !cfg_path.exists();
    let mut startup_errors = Vec::new();
    if first_run {
        if let Err(e) = std::fs::write(&cfg_path, config::DEFAULT_TOML) {
            startup_errors.push(format!("写入默认配置失败：{e}"));
        }
        if !portable && let Err(e) = autostart::set(true) {
            startup_errors.push(format!("设置开机自启失败：{e}"));
        }
    }
    let cfg = Config::load(&cfg_path).unwrap_or_else(|e| {
        startup_errors.push(format!("配置文件有误，已使用默认配置：\n{e}"));
        Config::default()
    });

    let (store, entries) = match Store::open(&data_dir.join("history.db"), cfg.general.max_items) {
        Ok(v) => v,
        Err(e) => {
            app::message_box(&format!("无法打开历史数据库：{e}"));
            return;
        }
    };

    app::run(app::Init { cfg, cfg_path, data_dir, store, entries, first_run, startup_errors });
}

//! 开机自启：写 HKCU\Software\Microsoft\Windows\CurrentVersion\Run，不需要管理员权限。

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;
use windows::core::{PCWSTR, w};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("clipstack");

fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\"", exe.display()))
}

struct Key(HKEY);

impl Key {
    fn open(access: REG_SAM_FLAGS) -> Result<Key, String> {
        let mut hkey = HKEY::default();
        let err = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut hkey) };
        if err != ERROR_SUCCESS {
            return Err(format!("打开注册表 Run 键失败（{}）", err.0));
        }
        Ok(Key(hkey))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// 注册表里是否有指向当前程序的自启项。
pub fn is_enabled() -> bool {
    let (Ok(key), Ok(cmd)) = (Key::open(KEY_QUERY_VALUE), command()) else {
        return false;
    };
    let mut size = 0u32;
    unsafe {
        if RegQueryValueExW(key.0, VALUE_NAME, None, None, None, Some(&mut size)) != ERROR_SUCCESS {
            return false;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        if RegQueryValueExW(key.0, VALUE_NAME, None, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
            != ERROR_SUCCESS
        {
            return false;
        }
        let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end]).eq_ignore_ascii_case(&cmd)
    }
}

pub fn set(enabled: bool) -> Result<(), String> {
    let key = Key::open(KEY_SET_VALUE)?;
    let err = if enabled {
        let data: Vec<u8> = command()?.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        unsafe { RegSetValueExW(key.0, VALUE_NAME, None, REG_SZ, Some(&data)) }
    } else {
        match unsafe { RegDeleteValueW(key.0, VALUE_NAME) } {
            e if e == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            e => e,
        }
    };
    if err != ERROR_SUCCESS {
        return Err(format!("写注册表失败（{}）", err.0));
    }
    Ok(())
}

//! Windows 剪贴板读写。
//!
//! 一条记录只取一种“主类型”：文件列表 > 文本（附带 HTML / RTF 富文本）> 图片（CF_DIB）。

use std::borrow::Cow;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{PWSTR, w};
use xxhash_rust::xxh3::Xxh3;

use crate::history::Kind;

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

#[derive(Clone, Debug, PartialEq)]
pub enum ClipData {
    Text { text: String, html: Option<Vec<u8>>, rtf: Option<Vec<u8>> },
    Image { dib: Vec<u8> },
    Files { paths: Vec<String> },
}

impl ClipData {
    pub fn kind(&self) -> Kind {
        match self {
            ClipData::Text { .. } => Kind::Text,
            ClipData::Image { .. } => Kind::Image,
            ClipData::Files { .. } => Kind::Files,
        }
    }

    /// 内容指纹，用来去重。只看主内容，同一段文字的不同富文本格式算同一条。
    pub fn key(&self) -> i64 {
        let mut h = Xxh3::new();
        match self {
            ClipData::Text { text, .. } => {
                h.update(b"T");
                h.update(text.as_bytes());
            }
            ClipData::Image { dib } => {
                h.update(b"I");
                h.update(dib);
            }
            ClipData::Files { paths } => {
                h.update(b"F");
                for p in paths {
                    h.update(p.as_bytes());
                    h.update(b"\0");
                }
            }
        }
        h.digest() as i64
    }

    pub fn size(&self) -> usize {
        match self {
            ClipData::Text { text, html, rtf } => {
                text.len() + html.as_ref().map_or(0, Vec::len) + rtf.as_ref().map_or(0, Vec::len)
            }
            ClipData::Image { dib } => dib.len(),
            ClipData::Files { paths } => paths.iter().map(String::len).sum(),
        }
    }

    pub fn preview(&self) -> String {
        match self {
            ClipData::Text { text, .. } => one_line(text, 200),
            ClipData::Image { dib } => match dib_size(dib) {
                Some((w, h)) => format!("[图片 {w}×{h}]"),
                None => "[图片]".into(),
            },
            ClipData::Files { paths } => {
                let first = paths.first().map(|p| p.rsplit('\\').next().unwrap_or(p)).unwrap_or("");
                if paths.len() > 1 {
                    format!("[文件] {first} 等 {} 个", paths.len())
                } else {
                    format!("[文件] {first}")
                }
            }
        }
    }

    /// 写进全文索引的内容。超长文本只索引前 SEARCH_MAX_CHARS 个字符。
    /// 与 store.rs 里迁移旧数据用的 SQL 保持一致。
    pub fn search_body(&self) -> Cow<'_, str> {
        match self {
            ClipData::Text { text, .. } => match text.char_indices().nth(SEARCH_MAX_CHARS) {
                Some((end, _)) => Cow::Borrowed(&text[..end]),
                None => Cow::Borrowed(text),
            },
            ClipData::Image { .. } => Cow::Borrowed("图片 image"),
            ClipData::Files { paths } => Cow::Owned(format!("文件 file {}", paths.join(" "))),
        }
    }
}

pub const SEARCH_MAX_CHARS: usize = 500_000;

/// 把多行文本压成一行并截断，用于列表展示。
fn one_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count >= max_chars {
                out.push('…');
                return out;
            }
            out.push(ch);
            count += 1;
        }
    }
    out
}

fn dib_size(dib: &[u8]) -> Option<(i32, i32)> {
    let w = i32::from_le_bytes(dib.get(4..8)?.try_into().ok()?);
    let h = i32::from_le_bytes(dib.get(8..12)?.try_into().ok()?);
    Some((w, h.abs()))
}

/// 运行时注册的剪贴板格式。
pub struct Formats {
    html: u32,
    rtf: u32,
    exclude: u32,
    can_include: u32,
    viewer_ignore: u32,
    drop_effect: u32,
}

impl Formats {
    pub fn register() -> Formats {
        unsafe {
            Formats {
                html: RegisterClipboardFormatW(w!("HTML Format")),
                rtf: RegisterClipboardFormatW(w!("Rich Text Format")),
                // 密码管理器等会设置这些格式，表示“不要被剪贴板历史记录”
                exclude: RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
                can_include: RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
                viewer_ignore: RegisterClipboardFormatW(w!("Clipboard Viewer Ignore")),
                drop_effect: RegisterClipboardFormatW(w!("Preferred DropEffect")),
            }
        }
    }
}

pub struct ReadOptions {
    pub images: bool,
    pub files: bool,
}

/// 打开剪贴板的 RAII 守卫；别的程序占用时重试几次。
struct Opened;

impl Opened {
    fn open(hwnd: HWND) -> Option<Opened> {
        for _ in 0..10 {
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                return Some(Opened);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn available(format: u32) -> bool {
    format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

fn get_bytes(format: u32) -> Option<Vec<u8>> {
    unsafe {
        let handle = GetClipboardData(format).ok()?;
        let mem = HGLOBAL(handle.0);
        let ptr = GlobalLock(mem) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, GlobalSize(mem)).to_vec();
        let _ = GlobalUnlock(mem);
        Some(bytes)
    }
}

fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

fn get_text() -> Option<String> {
    let bytes = get_bytes(CF_UNICODETEXT)?;
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

fn get_files() -> Option<Vec<String>> {
    unsafe {
        let handle = GetClipboardData(CF_HDROP).ok()?;
        let hdrop = HDROP(handle.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None);
            let mut buf = vec![0u16; len as usize + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(String::from_utf16_lossy(&buf[..got as usize]));
        }
        Some(paths)
    }
}

/// 读取当前剪贴板。被标记为“不记录”或没有支持的格式时返回 None。
pub fn read(hwnd: HWND, f: &Formats, opt: &ReadOptions) -> Option<ClipData> {
    let _guard = Opened::open(hwnd)?;
    if available(f.exclude) || available(f.viewer_ignore) {
        return None;
    }
    if available(f.can_include)
        && let Some(b) = get_bytes(f.can_include)
        && b.len() >= 4
        && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0
    {
        return None;
    }
    if opt.files
        && available(CF_HDROP)
        && let Some(paths) = get_files().filter(|p| !p.is_empty())
    {
        return Some(ClipData::Files { paths });
    }
    if available(CF_UNICODETEXT)
        && let Some(text) = get_text().filter(|t| !t.is_empty())
    {
        let html = available(f.html).then(|| get_bytes(f.html)).flatten().map(trim_nul);
        let rtf = available(f.rtf).then(|| get_bytes(f.rtf)).flatten().map(trim_nul);
        return Some(ClipData::Text { text, html, rtf });
    }
    if opt.images
        && available(CF_DIB)
        && let Some(dib) = get_bytes(CF_DIB).filter(|d| d.len() > 40)
    {
        return Some(ClipData::Image { dib });
    }
    None
}

fn set_bytes(format: u32, bytes: &[u8]) -> bool {
    unsafe {
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return false;
        };
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        // 成功后内存归系统所有；失败才需要自己释放
        if SetClipboardData(format, Some(HANDLE(mem.0))).is_err() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        true
    }
}

fn utf16z_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

fn with_nul(bytes: &[u8]) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v.push(0);
    v
}

fn dropfiles(paths: &[String]) -> Vec<u8> {
    // DROPFILES { pFiles: u32, pt: POINT, fNC: BOOL, fWide: BOOL }，后面跟双 NUL 结尾的 UTF-16 路径列表
    let mut v = Vec::new();
    v.extend_from_slice(&20u32.to_le_bytes());
    v.extend_from_slice(&[0u8; 12]);
    v.extend_from_slice(&1u32.to_le_bytes());
    for p in paths {
        v.extend(utf16z_bytes(p));
    }
    v.extend_from_slice(&[0, 0]);
    v
}

/// 把内容写进剪贴板。`plain` 时只写纯文本。
pub fn write(hwnd: HWND, f: &Formats, data: &ClipData, plain: bool) -> bool {
    let Some(_guard) = Opened::open(hwnd) else {
        return false;
    };
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    match data {
        ClipData::Text { text, html, rtf } => {
            let ok = set_bytes(CF_UNICODETEXT, &utf16z_bytes(text));
            if !plain {
                if let Some(h) = html {
                    set_bytes(f.html, &with_nul(h));
                }
                if let Some(r) = rtf {
                    set_bytes(f.rtf, &with_nul(r));
                }
            }
            ok
        }
        ClipData::Image { dib } => set_bytes(CF_DIB, dib),
        ClipData::Files { paths } if plain => set_bytes(CF_UNICODETEXT, &utf16z_bytes(&paths.join("\r\n"))),
        ClipData::Files { paths } => {
            let ok = set_bytes(CF_HDROP, &dropfiles(paths));
            // DROPEFFECT_COPY：粘贴到资源管理器时是复制而不是移动
            set_bytes(f.drop_effect, &1u32.to_le_bytes());
            ok
        }
    }
}

/// 当前剪贴板所有者的进程文件名，例如 "KeePass.exe"。
pub fn owner_exe() -> Option<String> {
    window_exe(unsafe { GetClipboardOwner() }.ok()?)
}

/// 窗口所属进程的文件名，例如 "Xshell.exe"。
pub fn window_exe(hwnd: HWND) -> Option<String> {
    if hwnd.is_invalid() {
        return None;
    }
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let res = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        res.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> ClipData {
        ClipData::Text { text: s.into(), html: None, rtf: None }
    }

    #[test]
    fn key_ignores_rich_formats_but_not_kind() {
        let plain = text("hello");
        let rich = ClipData::Text { text: "hello".into(), html: Some(b"<b>hello</b>".to_vec()), rtf: None };
        assert_eq!(plain.key(), rich.key());
        assert_ne!(plain.key(), text("hello!").key());
        assert_ne!(plain.key(), ClipData::Files { paths: vec!["hello".into()] }.key());
    }

    #[test]
    fn preview_is_single_line_and_truncated() {
        assert_eq!(text("  a\r\n\tb   c ").preview(), "a b c");
        let long = "x".repeat(500);
        let p = text(&long).preview();
        assert_eq!(p.chars().count(), 201);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn previews_images_and_files() {
        let mut dib = vec![0u8; 40];
        dib[4..8].copy_from_slice(&1920i32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-1080i32).to_le_bytes());
        assert_eq!(ClipData::Image { dib }.preview(), "[图片 1920×1080]");

        let files = ClipData::Files { paths: vec![r"C:\a\one.txt".into(), r"C:\b\two.txt".into()] };
        assert_eq!(files.preview(), "[文件] one.txt 等 2 个");
    }

    #[test]
    fn dropfiles_layout() {
        let v = dropfiles(&["ab".into()]);
        assert_eq!(&v[..4], &20u32.to_le_bytes());
        assert_eq!(&v[16..20], &1u32.to_le_bytes());
        // "ab\0" + 结尾 "\0"，UTF-16
        assert_eq!(&v[20..], &[b'a', 0, b'b', 0, 0, 0, 0, 0]);
    }
}

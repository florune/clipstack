//! 按键组合解析："Ctrl+Shift+V" 这类字符串 <-> 修饰键 + 虚拟键码。

use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

impl Combo {
    pub const CTRL_V: Combo = Combo { ctrl: true, alt: false, shift: false, win: false, vk: b'V' as u16 };

    pub fn parse(s: &str) -> Result<Combo, String> {
        let mut c = Combo { ctrl: false, alt: false, shift: false, win: false, vk: 0 };
        for part in s.split('+').map(str::trim) {
            if part.is_empty() {
                return Err(format!("无效的按键组合 {s:?}"));
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => c.ctrl = true,
                "alt" => c.alt = true,
                "shift" => c.shift = true,
                "win" | "super" | "meta" => c.win = true,
                name => {
                    if c.vk != 0 {
                        return Err(format!("{s:?} 里有多个主键"));
                    }
                    c.vk = vk_from_name(name).ok_or_else(|| format!("不认识的按键 {part:?}（在 {s:?} 里）"))?;
                }
            }
        }
        if c.vk == 0 {
            return Err(format!("{s:?} 缺少主键"));
        }
        Ok(c)
    }

    pub fn hotkey_modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut m = MOD_NOREPEAT;
        if self.ctrl {
            m |= MOD_CONTROL;
        }
        if self.alt {
            m |= MOD_ALT;
        }
        if self.shift {
            m |= MOD_SHIFT;
        }
        if self.win {
            m |= MOD_WIN;
        }
        m
    }
}

fn vk_from_name(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    if b.len() == 1 {
        let ch = b[0].to_ascii_uppercase();
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Some(ch as u16);
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok())
        && (1..=24).contains(&n)
    {
        return Some(VK_F1.0 + n - 1);
    }
    let vk = match name {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "space" => VK_SPACE,
        "backspace" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "plus" => VK_OEM_PLUS,
        "minus" => VK_OEM_MINUS,
        "comma" => VK_OEM_COMMA,
        "period" => VK_OEM_PERIOD,
        "backquote" | "grave" => VK_OEM_3,
        _ => return None,
    };
    Some(vk.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let c = Combo::parse("Ctrl+Shift+V").unwrap();
        assert!(c.ctrl && c.shift && !c.alt && !c.win);
        assert_eq!(c.vk, b'V' as u16);

        let c = Combo::parse(" win + alt + f12 ").unwrap();
        assert!(c.win && c.alt);
        assert_eq!(c.vk, VK_F12.0);

        assert_eq!(Combo::parse("tab").unwrap().vk, VK_TAB.0);
        assert_eq!(Combo::parse("Ctrl+1").unwrap().vk, b'1' as u16);
    }

    #[test]
    fn rejects_bad_combos() {
        assert!(Combo::parse("").is_err());
        assert!(Combo::parse("Ctrl+Shift").is_err());
        assert!(Combo::parse("Ctrl+A+B").is_err());
        assert!(Combo::parse("Ctrl+Nope").is_err());
        assert!(Combo::parse("Ctrl++V").is_err());
    }
}

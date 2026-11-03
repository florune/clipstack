//! TOML 配置：读取、默认值、宏编译与校验。

use std::path::Path;

use serde::Deserialize;

use crate::keys::Combo;

pub const DEFAULT_TOML: &str = include_str!("../assets/default_config.toml");

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub hotkeys: Hotkeys,
    pub paste: Paste,
    #[serde(rename = "macro")]
    pub macros: Vec<MacroDef>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub max_items: usize,
    pub max_item_mb: u64,
    pub record_images: bool,
    pub record_files: bool,
    pub restore_delay_ms: u32,
    pub macro_paste_delay_ms: u64,
    pub ignore_apps: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        General {
            max_items: 1000,
            max_item_mb: 64,
            record_images: true,
            record_files: true,
            restore_delay_ms: 300,
            macro_paste_delay_ms: 150,
            ignore_apps: ["KeePass.exe", "KeePassXC.exe", "1Password.exe", "Bitwarden.exe"].map(String::from).to_vec(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkeys {
    pub paste_previous: String,
    pub show_panel: String,
    pub toggle_pause: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { paste_previous: "Ctrl+Shift+V".into(), show_panel: "Ctrl+Alt+V".into(), toggle_pause: String::new() }
    }
}

/// clipstack 把内容写进剪贴板后，用什么按键让目标程序粘贴。
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paste {
    pub keys: String,
    pub terminal_keys: String,
    pub terminal_apps: Vec<String>,
}

impl Default for Paste {
    fn default() -> Self {
        Paste {
            keys: "Ctrl+V".into(),
            terminal_keys: "Shift+Insert".into(),
            terminal_apps: [
                "WindowsTerminal.exe",
                "OpenConsole.exe",
                "conhost.exe",
                "cmd.exe",
                "powershell.exe",
                "pwsh.exe",
                "putty.exe",
                "kitty.exe",
                "Xshell.exe",
                "MobaXterm.exe",
                "SecureCRT.exe",
                "mintty.exe",
                "Tabby.exe",
                "alacritty.exe",
                "wezterm-gui.exe",
                "WindTerm.exe",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacroDef {
    pub name: String,
    pub hotkey: String,
    pub steps: Vec<StepDef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepDef {
    Paste(usize),
    PastePlain(usize),
    Key(String),
    Text(String),
    Sleep(u64),
}

/// 校验过的宏，可以直接执行。
#[derive(Debug, Clone)]
pub struct Macro {
    pub name: String,
    pub hotkey: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `index` 从 1 开始：1 = 当前内容，2 = 上一条。
    Paste {
        index: usize,
        plain: bool,
    },
    Key(Combo),
    Text(String),
    Sleep(u64),
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        if cfg.general.max_items == 0 {
            return Err("general.max_items 必须大于 0".into());
        }
        Combo::parse(&cfg.paste.keys).map_err(|e| format!("paste.keys：{e}"))?;
        Combo::parse(&cfg.paste.terminal_keys).map_err(|e| format!("paste.terminal_keys：{e}"))?;
        cfg.compile_macros()?;
        Ok(cfg)
    }

    pub fn max_item_bytes(&self) -> usize {
        (self.general.max_item_mb as usize).saturating_mul(1024 * 1024)
    }

    /// 前台程序 `exe` 用的粘贴键：终端里 Ctrl+V 会被当成字符发给 shell，所以改用 Shift+Insert。
    pub fn paste_combo(&self, exe: Option<&str>) -> Combo {
        let is_terminal = exe.is_some_and(|exe| self.paste.terminal_apps.iter().any(|a| a.eq_ignore_ascii_case(exe)));
        let keys = if is_terminal { &self.paste.terminal_keys } else { &self.paste.keys };
        Combo::parse(keys).unwrap_or(Combo::CTRL_V)
    }

    pub fn compile_macros(&self) -> Result<Vec<Macro>, String> {
        self.macros
            .iter()
            .map(|m| {
                let ctx = |e: String| format!("宏 {:?}：{e}", m.name);
                Combo::parse(&m.hotkey).map_err(ctx)?;
                let steps = m
                    .steps
                    .iter()
                    .map(|s| match s {
                        StepDef::Paste(0) | StepDef::PastePlain(0) => {
                            Err("paste 的序号从 1 开始（1 = 当前内容）".to_string())
                        }
                        StepDef::Paste(n) => Ok(Step::Paste { index: *n, plain: false }),
                        StepDef::PastePlain(n) => Ok(Step::Paste { index: *n, plain: true }),
                        StepDef::Key(k) => Combo::parse(k).map(Step::Key),
                        StepDef::Text(t) => Ok(Step::Text(t.clone())),
                        StepDef::Sleep(ms) => Ok(Step::Sleep(*ms)),
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(ctx)?;
                Ok(Macro { name: m.name.clone(), hotkey: m.hotkey.clone(), steps })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_default_struct() {
        let cfg = Config::parse(DEFAULT_TOML).unwrap();
        let def = Config::default();
        assert_eq!(cfg.general.max_items, def.general.max_items);
        assert_eq!(cfg.general.ignore_apps, def.general.ignore_apps);
        assert_eq!(cfg.hotkeys.paste_previous, def.hotkeys.paste_previous);
        assert_eq!(cfg.hotkeys.show_panel, def.hotkeys.show_panel);
        assert_eq!(cfg.paste.keys, def.paste.keys);
        assert_eq!(cfg.paste.terminal_keys, def.paste.terminal_keys);
        assert_eq!(cfg.paste.terminal_apps, def.paste.terminal_apps);
        assert!(cfg.macros.is_empty());
    }

    #[test]
    fn terminals_use_shift_insert() {
        let cfg = Config::default();
        assert_eq!(cfg.paste_combo(Some("notepad.exe")), Combo::CTRL_V);
        assert_eq!(cfg.paste_combo(None), Combo::CTRL_V);
        let term = cfg.paste_combo(Some("xshell.EXE"));
        assert!(term.shift && !term.ctrl);
        assert_eq!(term, Combo::parse("Shift+Insert").unwrap());
        assert!(Config::parse("[paste]\nterminal_keys = 'Hyper+V'").is_err());
    }

    #[test]
    fn compiles_sequence_macro() {
        let cfg = Config::parse(
            r#"
            [[macro]]
            name = "填表"
            hotkey = "Ctrl+Alt+1"
            steps = [
              { paste = 2 },
              { key = "Tab" },
              { paste_plain = 1 },
              { text = "你好" },
              { sleep = 50 },
            ]
            "#,
        )
        .unwrap();
        let m = &cfg.compile_macros().unwrap()[0];
        assert_eq!(m.steps.len(), 5);
        assert_eq!(m.steps[0], Step::Paste { index: 2, plain: false });
        assert_eq!(m.steps[2], Step::Paste { index: 1, plain: true });
        assert_eq!(m.steps[3], Step::Text("你好".into()));
    }

    #[test]
    fn rejects_bad_config() {
        assert!(Config::parse("[general]\nmax_items = 0").is_err());
        assert!(Config::parse("[general]\ntypo = 1").is_err());
        let bad_step = "[[macro]]\nname='x'\nhotkey='Ctrl+Alt+1'\nsteps=[{ paste = 0 }]";
        assert!(Config::parse(bad_step).is_err());
        let bad_key = "[[macro]]\nname='x'\nhotkey='Ctrl+Alt+1'\nsteps=[{ key = 'Hyper' }]";
        assert!(Config::parse(bad_key).is_err());
    }
}

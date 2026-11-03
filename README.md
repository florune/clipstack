# clipstack

一个快、轻、常驻后台的 Windows 剪贴板历史工具，用 Rust 编写。

*A fast, lightweight, persistent clipboard history manager for Windows, written in Rust — global hotkeys, a searchable history panel, and sequence macros.*

- **Ctrl+V** 照常粘贴当前内容，**Ctrl+Shift+V** 粘贴上一条复制的内容
- **Ctrl+Alt+V** 打开历史面板：输入即搜索，回车粘贴
- 历史存到 SQLite，重启后还在；支持文本（保留 HTML / RTF 富文本格式）、图片、文件列表
- 序列宏：一个快捷键依次执行“粘贴第 N 条 / 按键 / 输入文字”，适合填表
- 开机自启、托盘图标、密码管理器内容不记录
- 单个 exe 约 2 MB，空闲时 CPU 占用为 0，私有内存约 3 MB

## 安装

从 [Actions](https://github.com/florune/clipstack/actions) 下载最新构建的 `clipstack.exe`，或自己编译：

```bash
cargo build --release
```

产物在 `target/release/clipstack.exe`。双击运行即可，首次运行会：

1. 在 `%APPDATA%\clipstack\config.toml` 生成默认配置
2. 打开开机自启（可在托盘菜单里关闭）
3. 弹出托盘提示

再次运行 exe 不会启动第二个实例，而是打开已运行实例的历史面板。

## 快捷键

| 快捷键 | 作用 |
|---|---|
| Ctrl+V | 粘贴当前内容（系统原生，clipstack 不拦截） |
| Ctrl+Shift+V | 粘贴上一条复制的内容，粘贴后剪贴板自动恢复成当前内容 |
| Ctrl+Alt+V | 打开 / 关闭历史面板 |

历史面板里：

| 按键 | 作用 |
|---|---|
| 直接输入 | 全文搜索全部历史（空格分隔多个关键词，需全部命中），最多显示 200 条 |
| ↑ ↓ / PageUp PageDown | 选择 |
| Enter / 双击 | 粘贴，该条目变成当前内容 |
| Shift+Enter | 只粘贴纯文本 |
| Ctrl+1 ~ Ctrl+9 | 直接粘贴列表中第 1~9 条 |
| Shift+Delete | 删除选中条目 |
| Esc | 关闭 |

所有全局快捷键都可以在配置文件里修改。

### 在终端 / SSH 客户端里粘贴

终端里的 Ctrl+V 不是粘贴，它会作为控制字符 `^V` 发给远端 shell（bash 的 quoted-insert），结果回车变成 `^M`、报 `command not found`。所以当前台是终端程序时，clipstack 改用 **Shift+Insert** 粘贴。

默认识别的终端：Windows Terminal、cmd、PowerShell、PuTTY、KiTTY、Xshell、MobaXterm、SecureCRT、mintty（Git Bash）、Tabby、Alacritty、WezTerm、WindTerm。用的终端不在列表里，就把它的进程名加到配置的 `[paste] terminal_apps` 里。

## 配置

托盘右键 →“编辑配置文件”，改完点“重新加载配置”即可生效，不用重启。完整的配置项说明见 [assets/default_config.toml](assets/default_config.toml)。

```toml
[general]
max_items = 1000          # 最多保留多少条
max_item_mb = 64          # 单条超过这个大小不记录
record_images = true
record_files = true
restore_delay_ms = 300    # 粘贴上一条后多久恢复剪贴板
ignore_apps = ["KeePass.exe", "1Password.exe"]

[hotkeys]
paste_previous = "Ctrl+Shift+V"
show_panel = "Ctrl+Alt+V"
toggle_pause = ""         # 留空表示不注册

[paste]
keys = "Ctrl+V"                    # 普通程序的粘贴键
terminal_keys = "Shift+Insert"     # 终端程序的粘贴键
terminal_apps = ["Xshell.exe", "putty.exe", "WindowsTerminal.exe"]  # 默认列表更长
```

### 序列宏

```toml
[[macro]]
name = "填两栏"
hotkey = "Ctrl+Alt+1"
steps = [
  { paste = 2 },          # 粘贴第 2 条历史（1 = 当前内容）
  { key = "Tab" },        # 按键，也可以是组合键 "Ctrl+A"
  { paste = 1 },
  { text = "（已填）" },   # 直接输入文字，不经过剪贴板
  { sleep = 100 },        # 等待毫秒
  { paste_plain = 3 },    # 只粘贴纯文本
]
```

宏执行完之后，剪贴板会恢复成执行前的当前内容。

## 数据与隐私

- 历史数据库：`%LOCALAPPDATA%\clipstack\history.db`（SQLite，图片 LZ4 压缩）
- 日志：`%LOCALAPPDATA%\clipstack\clipstack.log`
- 不记录以下内容：
  - 带有 `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory=0` / `Clipboard Viewer Ignore` 标记的内容（KeePass、1Password 等密码管理器复制密码时会设置这些标记）
  - `ignore_apps` 里列出的程序复制的内容
- 托盘菜单可以暂停记录、清空历史
- 设置环境变量 `CLIPSTACK_HOME=<目录>` 进入便携模式：配置和数据都放在该目录，首次运行也不写开机自启

## 工作原理

| 部分 | 实现 |
|---|---|
| 监听剪贴板 | `AddClipboardFormatListener`，事件驱动，不轮询 |
| 全局快捷键 | `RegisterHotKey` |
| 粘贴 | 写入剪贴板 → 等用户松开修饰键 → `SendInput` 模拟 Ctrl+V |
| 持久化 | SQLite（WAL），后台线程写入，不阻塞 UI |
| 去重 | 按内容的 xxh3 哈希去重，重复复制的条目移到最前 |
| 搜索 | SQLite FTS5 全文索引（trigram 分词，支持中文和任意子串，不区分大小写），按最近使用顺序取前 200 条；少于 3 个字的关键词在最近 5000 条里逐条匹配 |
| 内存 | 只缓存最近 500 条的预览，完整内容和索引都在数据库里，内存不随历史条数增长 |
| 历史面板 | 原生 Win32 窗口，提前创建、平时隐藏，唤出无需等待 |
| 开机自启 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，不需要管理员权限 |

## 已知限制

- 只支持 Windows 10 / 11。
- Ctrl+Shift+V 被注册为全局快捷键后，会覆盖其他程序里的同名快捷键（例如终端的粘贴、浏览器的“粘贴为纯文本”）。如有冲突，请在配置里换一个。
- clipstack 以普通权限运行时，无法向以管理员身份运行的窗口发送按键（Windows UIPI 限制）。
- 粘贴上一条之后，剪贴板恢复的是 clipstack 记录下来的格式（文本 + HTML + RTF / 图片 / 文件列表）；一些程序私有的格式（例如 Excel 的单元格对象）不会被恢复。

## 开发

```bash
cargo test
cargo clippy --all-targets
cargo fmt --check
```

搜索性能测试（写入 10 万条、每条约 600 字节，然后测搜索耗时）：

```bash
cargo test --release -- --ignored --nocapture bench_search
```

参考结果：每次搜索 0.1–3.3 ms；写入平均约 0.9 ms/条（在后台线程）；数据库约 315 MB（trigram 索引大约是原文的 4 倍）。

## 许可证

[Apache-2.0](LICENSE)

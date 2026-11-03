//! 配置：niri 风格的 KDL（`docs/adr/0006` 修订）。
//!
//! 约定：**解析失败绝不 panic**。`Config::load` 返回 `Result`，调用者（合成器）
//! 在热重载失败时保留上一份有效配置。
//!
//! 示例见仓库根目录的 `examples/config.kdl`；语法尽量贴近 niri：
//!
//! ```kdl
//! input {
//!     keyboard {
//!         xkb { layout "us,cn"; variant ",pinyin"; }
//!     }
//! }
//!
//! output "eDP-1" { scale 1.5; position x=0 y=0; }
//!
//! layout {
//!     default "bsp"
//!     default-column-width { proportion 0.5; }
//! }
//!
//! binds {
//!     Mod+Return { spawn "foot"; }
//!     Mod+Q { close-window; }
//! }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

mod kdl;

/// 顶层配置。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub general: General,
    pub layout: LayoutConfig,
    /// 键盘与输入法相关。
    pub input: InputConfig,
    /// 按输出名匹配的规则；没写到的输出用后端默认值。
    pub outputs: Vec<OutputConfig>,
    /// 键绑定表；为空时用内置默认表。
    pub binds: Vec<KeyBind>,
}

/// 输入配置。目前只用到键盘布局，后面手势/触摸板可以继续加。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputConfig {
    pub keyboard: KeyboardConfig,
}

/// xkb 键盘布局。`layout "us,cn"`、`variant ",pinyin"` 直接交给 xkbcommon。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyboardConfig {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
    /// 按键重复延迟（毫秒，niri 同名选项）。
    pub repeat_delay: i32,
    /// 每秒重复次数。
    pub repeat_rate: i32,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            rules: String::new(),
            model: String::new(),
            layout: String::new(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: 200,
            repeat_rate: 25,
        }
    }
}

/// 按输出名生效的规则；niri 风格 `output "eDP-1" { ... }`。
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    /// `wl_output` 名字，例如 `eDP-1`、`HDMI-A-1`。
    pub name: String,
    /// 关闭这个输出（热插拔仍然记录，但不建 DRM 表面）。
    pub enabled: bool,
    /// 逻辑缩放；`None` 表示沿用后端/EDID 默认。
    pub scale: Option<f64>,
    /// 桌面坐标（逻辑像素）。
    pub position: Option<(i32, i32)>,
    /// 模式字符串 `1920x1080@60` 或 `1920x1080`；`None` 用首选模式。
    pub mode: Option<String>,
    /// `normal`、`90`、`180`、`270`、`flipped`、`flipped-90` ...
    pub transform: Option<String>,
    /// 启动时把指针放到这个输出上（只对第一个主输出有意义）。
    pub primary: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            scale: None,
            position: None,
            mode: None,
            transform: None,
            primary: false,
        }
    }
}

/// 全局项。字段命名沿用内部 snake_case，KDL 里同时接受 niri 的 kebab-case。
#[derive(Debug, Clone, PartialEq)]
pub struct General {
    /// bar 高度 = 字体行高 × 该倍数（基线 `barheight`，默认 1.1；0 表示用默认值）。
    pub bar_height: f32,
    pub bar_position: BarPosition,
    /// 是否显示 bar（基线 `hidebar` 的反面）。
    pub bar_enabled: bool,
    /// bar 字体大小（逻辑像素）。
    pub font_size: f32,
    /// bar 字体族名；留空用 fontconfig 的 monospace 别名。
    pub font_family: Option<String>,
    /// 是否给窗口画标题条（基线 `titlebar`）。
    pub titlebar: bool,
    /// 标题条内边距（逻辑像素，基线 `titlepadding`）。
    pub title_padding: i32,
    /// 标题文字居中（基线 `titlecenter`）；false 时左对齐。
    pub title_center: bool,
    /// 悬停时显示关闭按钮（基线 `titleclose`）。
    pub title_close: bool,
    /// 关闭按钮颜色 `#RRGGBB` / `#RRGGBBAA`（基线 `titleclosecolor`）。
    pub title_close_color: Option<String>,
    /// bar 中间是否再显示一次焦点窗口标题（基线 `barwintitle`，默认关）。
    pub bar_window_title: bool,
    /// 终端命令，`MOD+Enter` 用。
    pub terminal: String,
    /// 浮动窗口拖动时的吸附距离（px），0 关闭（基线 `movesnap`）。
    pub move_snap: i32,
    /// 从标题条拖动窗口前要超过的位移（px，基线 `movethreshold`）。
    pub move_threshold: f64,
    /// 是否接管 `org.freedesktop.Notifications` 并在 bar 上显示通知。
    pub show_notifications: bool,
    /// 通知默认停留时间（秒，基线 `notification_timeout`；客户端可覆盖）。
    pub notification_timeout: u32,
    /// 通知左侧进度条宽度（逻辑像素，基线 `notification_linewidth`；0 关闭）。
    pub notification_line_width: u32,
    /// 通知进度条圆角（逻辑像素，基线 `notification_lineradius`）。
    pub notification_line_radius: u32,
    /// 有动作的通知在右侧显示的提示（基线 `notification_actionsign`）。
    pub notification_action_sign: String,
    /// runner 空输入时的提示文字（基线 `runner_placeholder`）。
    pub runner_placeholder: String,
    /// 候选列表一次显示几条；0 表示不显示列表（基线 `runner_max_candidates`）。
    pub runner_max_candidates: usize,
    /// 是否允许模糊匹配（基线 `runner_fuzzy`）。
    pub runner_fuzzy: bool,
    /// 历史条数上限（基线 `runner_history_size`）。
    pub runner_history_size: usize,
    /// 计算结果的有效位数（基线 `runner_calc_precision`）。
    pub runner_calc_precision: u32,
    /// 执行命令用的 shell（基线 `runner_shell`）。
    pub runner_shell: String,
    /// `!` 模式用的终端；空串关掉该模式（基线 `runner_terminal`）。
    pub runner_terminal: String,
    /// `?` 模式的搜索 URL，`%s` 是查询词（基线 `runner_searchurl`）。
    pub runner_search_url: String,
    /// 是否启用文件模式（基线 `runner_files`）。
    pub runner_files: bool,
    /// 额外加进命令搜索的目录（基线 `runner_paths`）。
    pub runner_paths: String,
    /// 提示行与候选是否显示图标（基线 `runner_icons`）。
    pub runner_icons: RunnerIcons,
    /// 候选图标边长（逻辑像素，基线 `runner_iconsize`；0 用行高）。
    pub runner_icon_size: u32,
    /// 缺图标占位方块的圆角比例 0-1（基线 `runner_iconround`）。
    pub runner_icon_round: f32,
    /// 缺图标占位方块的边框宽度（逻辑像素，基线 `runner_iconborder`）。
    pub runner_icon_border: u32,
    /// 选中候选胶囊的高度（逻辑像素，基线 `runner_pillheight`；0 用 4/3 行高）。
    pub runner_pill_height: u32,
    /// 选中候选胶囊的左右内边距（逻辑像素，基线 `runner_pillpadding`；0 自动）。
    pub runner_pill_padding: u32,
    /// 选中候选胶囊的圆角比例 0-1（基线 `runner_pillround`；负数按 1 处理）。
    pub runner_pill_round: f32,
    pub runner_run_prefix: String,
    pub runner_term_prefix: String,
    pub runner_calc_prefix: String,
    pub runner_search_prefix: String,
    pub runner_window_prefix: String,
    pub runner_tag_prefix: String,
    /// 是否在 bar 右端显示 systray（基线 `showsystray`）。
    pub show_systray: bool,
    /// 两个托盘图标之间的间距（逻辑像素，基线 `systrayspacing`）。
    pub systray_spacing: i32,
    /// 托盘两端的内边距（逻辑像素，基线 `systraypadding`）。
    pub systray_padding: i32,
    /// 托盘图标边长（逻辑像素，基线 `systrayiconsize`；0 表示填满 bar）。
    pub systray_icon_size: u32,
    /// bar 状态命令；stdout 的每一行刷新右侧状态文本（类似 sway/scroll 的 status_command）。
    pub status_command: Option<String>,
    /// 启动时执行的命令。
    pub autostart: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            bar_height: 1.1,
            bar_position: BarPosition::Top,
            bar_enabled: true,
            font_size: 12.0,
            font_family: None,
            titlebar: true,
            title_padding: 6,
            title_center: true,
            title_close: true,
            title_close_color: None,
            bar_window_title: false,
            terminal: "foot".into(),
            move_snap: 12,
            move_threshold: 4.0,
            show_notifications: true,
            notification_timeout: 5,
            notification_line_width: 4,
            notification_line_radius: 2,
            notification_action_sign: "[r]".into(),
            runner_placeholder: "Run...".into(),
            runner_max_candidates: 5,
            runner_fuzzy: true,
            runner_history_size: 100,
            runner_calc_precision: 10,
            runner_shell: "/bin/sh".into(),
            runner_terminal: "foot".into(),
            runner_search_url: "https://duckduckgo.com/?q=%s".into(),
            runner_files: true,
            runner_paths: String::new(),
            runner_icons: RunnerIcons::Selected,
            runner_icon_size: 0,
            runner_icon_round: 0.4,
            runner_icon_border: 1,
            runner_pill_height: 1,
            runner_pill_padding: 1,
            runner_pill_round: -1.0,
            runner_run_prefix: "!!".into(),
            runner_term_prefix: "!".into(),
            runner_calc_prefix: "=".into(),
            runner_search_prefix: "?".into(),
            runner_window_prefix: "@".into(),
            runner_tag_prefix: "#".into(),
            show_systray: true,
            systray_spacing: 2,
            systray_padding: 2,
            systray_icon_size: 16,
            status_command: None,
            autostart: Vec::new(),
        }
    }
}

impl General {
    /// 把扁平的 `runner_*` 配置映射成 runner 自己的配置。
    pub fn runner_config(&self) -> crate::runner::Config {
        crate::runner::Config {
            placeholder: self.runner_placeholder.clone(),
            max_candidates: self.runner_max_candidates,
            fuzzy: self.runner_fuzzy,
            history_size: self.runner_history_size,
            calc_precision: self.runner_calc_precision,
            shell: self.runner_shell.clone(),
            terminal: self.runner_terminal.clone(),
            search_url: self.runner_search_url.clone(),
            files: self.runner_files,
            paths: self.runner_paths.clone(),
            prefixes: crate::runner::Prefixes {
                run: self.runner_run_prefix.clone(),
                term: self.runner_term_prefix.clone(),
                calc: self.runner_calc_prefix.clone(),
                search: self.runner_search_prefix.clone(),
                window: self.runner_window_prefix.clone(),
                tag: self.runner_tag_prefix.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BarPosition {
    #[default]
    Top,
    Bottom,
}

/// runner 的图标显示方式（基线 `RunnerIconsNone/Selected/All`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunnerIcons {
    /// 不留图标位。
    None,
    /// 只给选中的候选和提示行画图标。
    #[default]
    Selected,
    /// 每条候选都画，缺图标的用占位方块。
    All,
}

/// 布局项。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutConfig {
    /// 默认平铺布局（ADR 0003 修订：bsp）。
    pub default: LayoutName,
    /// 新列的宽度比例。
    pub default_width: f64,
    /// `cycle_size` 循环的宽度档位。
    pub widths: Vec<f64>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            default: LayoutName::Bsp,
            default_width: 0.5,
            widths: vec![1.0 / 3.0, 0.5, 2.0 / 3.0, 1.0],
        }
    }
}

/// 配置里的布局名。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutName {
    #[default]
    Bsp,
    Scroll,
    Tabbed,
}

impl From<LayoutName> for crate::layout::TiledLayout {
    fn from(name: LayoutName) -> Self {
        match name {
            LayoutName::Bsp => Self::Bsp,
            LayoutName::Scroll => Self::Scroll,
            LayoutName::Tabbed => Self::Tabbed,
        }
    }
}

/// 一条键绑定，动作名由动作表解析（`CONTEXT.md`「动作表」）。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyBind {
    /// 修饰键，如 `["super"]`、`["super", "shift"]`。
    pub mods: Vec<String>,
    /// 键名，按 xkb 命名，如 `"h"`、`"1"`。
    pub key: String,
    pub action: String,
    /// 动作参数，按动作表定义解释。
    pub arg: Option<String>,
}

impl Config {
    /// 默认路径：`$XDG_CONFIG_HOME/r0wm/config.kdl`。
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("r0wm").join("config.kdl")
    }

    /// 从 KDL 文本解析（单测与 IPC 调试用）。
    pub fn parse_str(text: &str) -> Result<Self> {
        kdl::parse(text)
    }

    /// 文件不存在时返回默认配置；存在但解析失败时返回错误（绝不 panic）。
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("读取配置失败：{}", path.display()));
            }
        };

        kdl::parse(&text).with_context(|| format!("解析 KDL 配置失败：{}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        kdl::parse(text).unwrap()
    }

    #[test]
    fn empty_config_is_default() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("// 只有注释\n").layout.default, LayoutName::Bsp);
    }

    #[test]
    fn niri_style_keybinds_parse() {
        let config = parse(
            r#"
            binds {
                Mod+Return { spawn "foot"; }
                Mod+Shift+Q { quit; }
                Mod+H { focus-column-left; }
                Mod+1 { focus-workspace 1; }
            }
            "#,
        );
        assert_eq!(config.binds.len(), 4);
        assert_eq!(config.binds[0].mods, ["super"]);
        assert_eq!(config.binds[0].key, "Return");
        assert_eq!(config.binds[0].action, "spawn");
        assert_eq!(config.binds[0].arg.as_deref(), Some("foot"));
        assert_eq!(config.binds[1].action, "quit");
        assert_eq!(config.binds[2].action, "focus_column_left");
        assert_eq!(config.binds[3].action, "view");
        assert_eq!(config.binds[3].arg.as_deref(), Some("1"));
    }

    #[test]
    fn general_bar_and_runner_sections() {
        let config = parse(
            r##"
            general {
                terminal "kitty"
                bar {
                    enabled false
                    position "bottom"
                    font-size 16
                    font-family "Maple Mono NF"
                    window-title true
                }
                titlebar {
                    enabled false
                    padding 10
                    center false
                    close false
                    close-color "#ff5f57"
                }
                notifications { enabled false; timeout 8; }
                runner { placeholder "Go..."; fuzzy false; icons "all"; }
                systray { enabled false; spacing 4; }
                autostart "foot"
            }
            "##,
        );
        assert_eq!(config.general.terminal, "kitty");
        assert!(!config.general.bar_enabled);
        assert_eq!(config.general.bar_position, BarPosition::Bottom);
        assert_eq!(config.general.font_size, 16.0);
        assert_eq!(config.general.font_family.as_deref(), Some("Maple Mono NF"));
        assert!(config.general.bar_window_title);
        assert!(!config.general.titlebar);
        assert_eq!(config.general.title_padding, 10);
        assert!(!config.general.title_center);
        assert!(!config.general.title_close);
        assert!(!config.general.show_notifications);
        assert_eq!(config.general.notification_timeout, 8);
        assert_eq!(config.general.runner_placeholder, "Go...");
        assert!(!config.general.runner_fuzzy);
        assert_eq!(config.general.runner_icons, RunnerIcons::All);
        assert!(!config.general.show_systray);
        assert_eq!(config.general.systray_spacing, 4);
        assert_eq!(config.general.autostart, ["foot"]);
    }

    #[test]
    fn layout_and_input_parse() {
        let config = parse(
            r#"
            input {
                keyboard {
                    xkb { layout "us,cn"; variant ",pinyin"; options "grp:alt_shift_toggle"; }
                    repeat-delay 300
                    repeat-rate 40
                }
            }
            layout {
                default "scroll"
                default-column-width { proportion 0.6; }
                preset-column-widths { proportion 0.33333; proportion 0.5; }
            }
            "#,
        );
        assert_eq!(config.input.keyboard.layout, "us,cn");
        assert_eq!(config.input.keyboard.variant, ",pinyin");
        assert_eq!(config.input.keyboard.repeat_delay, 300);
        assert_eq!(config.input.keyboard.repeat_rate, 40);
        assert_eq!(config.layout.default, LayoutName::Scroll);
        assert_eq!(config.layout.default_width, 0.6);
        assert_eq!(config.layout.widths, [0.33333, 0.5]);
    }

    #[test]
    fn outputs_parse() {
        let config = parse(
            r#"
            output "eDP-1" {
                scale 1.5
                position x=0 y=0
                mode "1920x1080@60"
                primary
            }
            output "HDMI-A-1" {
                enabled false
                transform "90"
            }
            "#,
        );
        assert_eq!(config.outputs.len(), 2);
        assert_eq!(config.outputs[0].name, "eDP-1");
        assert_eq!(config.outputs[0].scale, Some(1.5));
        assert_eq!(config.outputs[0].position, Some((0, 0)));
        assert_eq!(config.outputs[0].mode.as_deref(), Some("1920x1080@60"));
        assert!(config.outputs[0].primary);
        assert!(!config.outputs[1].enabled);
        assert_eq!(config.outputs[1].transform.as_deref(), Some("90"));
    }

    #[test]
    fn example_config_parses() {
        let config = Config::parse_str(include_str!("../../examples/config.kdl")).unwrap();
        assert_eq!(config.layout.default, LayoutName::Bsp);
        assert!(!config.binds.is_empty());
        assert_eq!(config.outputs.len(), 2);
        assert_eq!(config.input.keyboard.layout, "us,cn");
    }

    #[test]
    fn missing_file_uses_default() {
        let config = Config::load(Path::new("/nonexistent/r0wm/config.kdl")).unwrap();
        assert_eq!(config, Config::default());
    }
}

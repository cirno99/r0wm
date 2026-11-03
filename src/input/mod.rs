//! 输入：键盘、指针、触摸、手势。
//!
//! 关键点（`CONTEXT.md`「空间导航」）：方向键只解释为「该方向上的下一个东西」，
//! 不是线性焦点链；列内 tab 用 `Tab`/`Shift+Tab`。
//!
//! 本模块的 [`Binds`] 是键绑定表的运行时形态：配置里的键**名**在启动时解析成
//! keysym，避免每个按键都做字符串比较。
//!
//! TODO(M1)：指针（焦点跟随、拖动/缩放 grab）。
//! TODO(M7)：触摸板手势、数位板、IME 的 grab 转发。

use smithay::input::keyboard::{Keysym, ModifiersState, keysyms, xkb};

use crate::action::{Action, UnknownAction};
use crate::config::{Config, KeyBind};

/// 键盘修饰键集合，与配置里的 `mods` 名字对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub super_key: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Modifiers {
    /// 解析配置里的修饰键名字；出现未知名字时返回 `None`。
    pub fn parse(names: &[String]) -> Option<Self> {
        let mut mods = Self::default();
        for name in names {
            match name.as_str() {
                "super" | "logo" | "mod4" => mods.super_key = true,
                "shift" => mods.shift = true,
                "ctrl" | "control" => mods.ctrl = true,
                "alt" | "mod1" => mods.alt = true,
                _ => return None,
            }
        }
        Some(mods)
    }

    /// 事件里的修饰键状态是否与绑定完全一致（只比较我们跟踪的四个键）。
    pub fn matches(self, state: &ModifiersState) -> bool {
        self.super_key == state.logo
            && self.shift == state.shift
            && self.ctrl == state.ctrl
            && self.alt == state.alt
    }
}

/// 一条编译成 keysym 的绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub mods: Modifiers,
    pub key: Keysym,
    pub action: Action,
}

/// 键绑定表。
#[derive(Debug, Clone, Default)]
pub struct Binds {
    binds: Vec<Bind>,
}

impl Binds {
    /// 从配置构造。`binds` 为空时使用内置默认表（见 [`Binds::defaults`]）。
    pub fn from_config(config: &Config) -> Self {
        if config.binds.is_empty() {
            return Self::defaults(&config.general.terminal);
        }

        let mut binds = Vec::with_capacity(config.binds.len());
        for bind in &config.binds {
            match compile(bind) {
                None => continue,
                Some(Ok(bind)) => binds.push(bind),
                Some(Err(err)) => {
                    // 配置写错时宁可不绑定，也不猜用户意图；整张表作废并告警。
                    tracing::warn!("忽略整张键绑定表：{err}");
                    return Self::default();
                }
            }
        }
        Self { binds }
    }

    /// 内置默认表：`super` 系绑定，与 g0wm 的习惯一致。
    pub fn defaults(terminal: &str) -> Self {
        let entries: [(&[&str], &str, &str, Option<&str>); 27] = [
            (&["super"], "h", "focus_column_left", None),
            (&["super"], "l", "focus_column_right", None),
            (&["super"], "k", "focus_row_up", None),
            (&["super"], "j", "focus_row_down", None),
            (&["super"], "o", "focus_output", None),
            (&["super", "shift"], "o", "move_output", None),
            (&["super"], "Tab", "focus_next", None),
            (&["super"], "Return", "spawn", Some(terminal)),
            (&["super"], "v", "toggle_floating", None),
            (&["super"], "f", "toggle_fullscreen", None),
            (&["super", "shift"], "h", "consume_column_left", None),
            (&["super", "shift"], "l", "consume_column_right", None),
            (&["super", "shift"], "c", "expel", None),
            (&["super"], "s", "toggle_sticky", None),
            (&["super"], "q", "set_layout", Some("scroll")),
            (&["super"], "b", "set_layout", Some("bsp")),
            (&["super", "shift"], "q", "quit", None),
            (&["super", "shift"], "e", "reload", None),
            (&["super", "shift"], "t", "toggle_titlebar", None),
            (&["super"], "n", "notify_scroll", None),
            (&["super", "shift"], "n", "notify_open", None),
            (&["super", "ctrl"], "n", "notify_next", None),
            (&["super", "alt"], "n", "notify_prev", None),
            (&["super", "ctrl", "shift"], "n", "notify_dismiss_all", None),
            (&["super"], "a", "notify_actions", None),
            (&["super"], "r", "runner_toggle", None),
            (&["super"], "1", "view", Some("1")),
        ];

        let binds = entries
            .into_iter()
            .filter_map(|(mods, key, action, arg)| {
                let mods = mods.iter().map(|m| m.to_string()).collect::<Vec<_>>();
                let bind = KeyBind {
                    mods,
                    key: key.to_string(),
                    action: action.to_string(),
                    arg: arg.map(str::to_owned),
                };
                compile(&bind).and_then(|result| result.ok())
            })
            .collect();

        Self { binds }
    }

    /// 按键按下时查表。`syms` 用未修饰的 keysym（布局第 0 层），
    /// 这样 `Shift+h` 也能命中配置里的 `"h"`。
    pub fn lookup(&self, mods: &ModifiersState, syms: &[Keysym]) -> Option<&Action> {
        self.binds
            .iter()
            .find(|bind| bind.mods.matches(mods) && syms.contains(&bind.key))
            .map(|bind| &bind.action)
    }

    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }
}

fn compile(bind: &KeyBind) -> Option<Result<Bind, UnknownAction>> {
    let Some(mods) = Modifiers::parse(&bind.mods) else {
        tracing::warn!(key = %bind.key, mods = ?bind.mods, "未知修饰键，跳过该绑定");
        return None;
    };

    let key = xkb::keysym_from_name(&bind.key, xkb::KEYSYM_CASE_INSENSITIVE);
    if key.raw() == keysyms::KEY_NoSymbol {
        tracing::warn!(key = %bind.key, "未知键名，跳过该绑定");
        return None;
    }

    match Action::parse(&bind.action, bind.arg.as_deref()) {
        Ok(action) => Some(Ok(Bind { mods, key, action })),
        Err(err) => Some(Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::input::keyboard::xkb::KEYSYM_CASE_INSENSITIVE;

    fn config_with(binds: &str) -> Config {
        Config::parse_str(binds).unwrap()
    }

    #[test]
    fn super_matches_logo_only() {
        let bind = Modifiers {
            super_key: true,
            ..Default::default()
        };
        let mut state = ModifiersState::default();
        assert!(!bind.matches(&state));
        state.logo = true;
        assert!(bind.matches(&state));
        state.shift = true;
        assert!(!bind.matches(&state));
    }

    #[test]
    fn unknown_modifier_is_rejected() {
        assert_eq!(Modifiers::parse(&["hyper".into()]), None);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let binds = Binds::from_config(&Config::default());
        assert!(!binds.is_empty());
        let sym = xkb::keysym_from_name("q", KEYSYM_CASE_INSENSITIVE);
        let mods = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(binds.lookup(&mods, &[sym]), Some(&Action::Quit));

        let o = xkb::keysym_from_name("o", KEYSYM_CASE_INSENSITIVE);
        let mods = ModifiersState {
            logo: true,
            ..Default::default()
        };
        assert_eq!(binds.lookup(&mods, &[o]), Some(&Action::FocusOutput));

        let mods = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(binds.lookup(&mods, &[o]), Some(&Action::MoveOutput));
    }

    #[test]
    fn configured_bind_is_found() {
        let config = config_with(
            r#"
            binds {
                Mod+x { spawn "foot"; }
            }
            "#,
        );
        let binds = Binds::from_config(&config);
        let sym = xkb::keysym_from_name("x", KEYSYM_CASE_INSENSITIVE);
        let mods = ModifiersState {
            logo: true,
            ..Default::default()
        };
        assert_eq!(
            binds.lookup(&mods, &[sym]),
            Some(&Action::Spawn(vec!["foot".into()]))
        );
    }

    #[test]
    fn shift_h_uses_unmodified_sym() {
        let config = config_with(
            r#"
            binds {
                Mod+Shift+h { focus-column-left; }
            }
            "#,
        );
        let binds = Binds::from_config(&config);
        let sym = xkb::keysym_from_name("h", KEYSYM_CASE_INSENSITIVE);
        let mods = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(binds.lookup(&mods, &[sym]), Some(&Action::FocusColumnLeft));
    }
}

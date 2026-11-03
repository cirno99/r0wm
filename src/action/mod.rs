//! 动作表：键绑定、IPC 命令、bar 点击共用的唯一入口（`docs/adr/0007`）。
//!
//! 这里只做「名字 + 参数 → [`Action`]」的解析，不接触合成器状态，
//! 因此可以单测。

use crate::layout::{Direction, TiledLayout};

/// 一个可执行动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 只看某个 tag（位掩码）。
    View(u32),
    /// 切换某个 tag 的可见性。
    ToggleView(u32),
    /// 把焦点窗口移到某个 tag（位掩码，等同基线 `tag`）。
    TransferTo(u32),
    /// 切换焦点窗口的 tag 归属（异或，基线 `toggletag`）。
    ToggleTag(u32),
    /// 在当前可见集合与上一套可见集合之间切换（基线 `tagset[2]` 交替）。
    ViewPrev,
    /// 可见集合整体向前/向后平移一个 tag（基线 `shiftview`，bar 上滚轮用它）。
    ShiftView(i32),
    /// 切换平铺布局。
    SetLayout(TiledLayout),
    /// 行内向左/右移动焦点（空间导航）。
    FocusColumnLeft,
    FocusColumnRight,
    /// 在可见行之间移动焦点（bsp 下同时承担屏幕几何上的上下导航）。
    FocusRowUp,
    FocusRowDown,
    /// 焦点循环到下一个输出。
    FocusOutput,
    /// 把焦点窗口移到下一个输出。
    MoveOutput,
    /// 列内 tab 循环。
    TabNext,
    TabPrev,
    /// 焦点按最近使用顺序循环（浮层窗口只有这一种导航）。
    FocusNext,
    FocusPrev,
    /// 与浮动层互转。
    ToggleFloating,
    /// 焦点窗口进入/退出全屏。
    ToggleFullscreen,
    /// 把焦点窗口并入相邻列，成为该列 tab 组的新栈顶（scroll）。
    Consume(Direction),
    /// 把焦点 tab 从当前列拆出来，放到右侧新列（scroll）。
    Expel,
    /// 浮动窗口置顶并使其在所有 tag 可见（sticky）。
    ToggleSticky,
    /// 显示/隐藏每窗口标题条（基线 `toggletitlebar`）。
    ToggleTitlebar,
    /// 打开通知（`default` 动作；浏览历史时回到实时通知）。
    NotifyOpen,
    /// 关掉当前通知；历史视图里删掉这一条。
    NotifyDismiss,
    /// 当前与排队的通知全部关掉。
    NotifyDismissAll,
    /// 历史里向新的一条走；在最新的那条上则关掉实时通知。
    NotifyNext,
    /// 历史里向旧的一条走。
    NotifyPrev,
    /// 通知正文翻到下一屏。
    NotifyScroll,
    /// 展开/收起通知的动作选择器。
    NotifyActions,
    /// 打开/收起命令提示（基线 `runnertoggle`）。
    RunnerToggle,
    /// 启动命令（argv，不经过 shell）。
    Spawn(Vec<String>),
    /// 请求关闭焦点窗口（基线 `kill`）。
    Close,
    /// 重新加载配置。
    Reload,
    /// 退出合成器。
    Quit,
}

/// 解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAction(pub String);

impl std::fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "未知动作：{}", self.0)
    }
}

impl Action {
    /// 从配置/IPC 的动作名与参数构造动作。
    pub fn parse(name: &str, arg: Option<&str>) -> Result<Self, UnknownAction> {
        let action = match name {
            "view" => Self::View(tag_mask(arg, name)?),
            "toggle_view" => Self::ToggleView(tag_mask(arg, name)?),
            "transfer_to" => Self::TransferTo(tag_mask(arg, name)?),
            "toggle_tag" | "toggletag" => Self::ToggleTag(tag_mask(arg, name)?),
            "view_prev" | "viewprev" => Self::ViewPrev,
            "shift_view" | "shiftview" => Self::ShiftView(
                arg.and_then(|value| value.trim().parse().ok())
                    .ok_or_else(|| UnknownAction(name.to_string()))?,
            ),
            "set_layout" => Self::SetLayout(match arg {
                Some("bsp") => TiledLayout::Bsp,
                Some("scroll") => TiledLayout::Scroll,
                Some("tabbed") => TiledLayout::Tabbed,
                _ => return Err(UnknownAction(name.to_string())),
            }),
            "focus_column_left" => Self::FocusColumnLeft,
            "focus_column_right" => Self::FocusColumnRight,
            "focus_row_up" => Self::FocusRowUp,
            "focus_row_down" => Self::FocusRowDown,
            "focus_output" | "focus_output_next" | "focusoutput" | "focusoutputnext" => {
                Self::FocusOutput
            }
            "move_output"
            | "move_output_next"
            | "move_window_to_output"
            | "move_window_to_output_next" => Self::MoveOutput,
            "tab_next" => Self::TabNext,
            "tab_prev" => Self::TabPrev,
            "focus_next" => Self::FocusNext,
            "focus_prev" => Self::FocusPrev,
            "consume_column_left" | "consume_left" => Self::Consume(Direction::Left),
            "consume_column_right" | "consume_right" => Self::Consume(Direction::Right),
            "consume" | "consume_window" => match arg {
                Some("right") => Self::Consume(Direction::Right),
                _ => Self::Consume(Direction::Left),
            },
            "expel" | "expel_window" => Self::Expel,
            "toggle_floating" => Self::ToggleFloating,
            "toggle_fullscreen" | "fullscreen" => Self::ToggleFullscreen,
            "toggle_sticky" | "sticky" => Self::ToggleSticky,
            "toggle_titlebar" | "toggletitlebar" => Self::ToggleTitlebar,
            "notify_open" | "notifyopen" => Self::NotifyOpen,
            "notify_dismiss" | "notifydismiss" => Self::NotifyDismiss,
            "notify_dismiss_all" | "notifydismissall" => Self::NotifyDismissAll,
            "notify_next" | "notifynext" => Self::NotifyNext,
            "notify_prev" | "notifyprev" => Self::NotifyPrev,
            "notify_scroll" | "notifyscroll" => Self::NotifyScroll,
            "notify_actions" | "notifyactions" => Self::NotifyActions,
            "runner_toggle" | "runnertoggle" => Self::RunnerToggle,
            "close" | "kill" => Self::Close,
            "reload" => Self::Reload,
            "quit" => Self::Quit,
            "spawn" => {
                let argv: Vec<String> = arg
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                if argv.is_empty() {
                    return Err(UnknownAction(name.to_string()));
                }
                Self::Spawn(argv)
            }
            _ => return Err(UnknownAction(name.to_string())),
        };
        Ok(action)
    }
}

/// tag 参数是 1 起的序号，配置里也接受 `"1-3"` 这种区间。
fn tag_mask(arg: Option<&str>, name: &str) -> Result<u32, UnknownAction> {
    let raw = arg.ok_or_else(|| UnknownAction(name.to_string()))?;
    let mut mask = 0u32;
    for part in raw.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((from, to)) => {
                let from: u32 = from.parse().map_err(|_| UnknownAction(name.to_string()))?;
                let to: u32 = to.parse().map_err(|_| UnknownAction(name.to_string()))?;
                for tag in from..=to {
                    mask |= tag_bit(tag)?;
                }
            }
            None => mask |= tag_bit(part.parse().map_err(|_| UnknownAction(name.to_string()))?)?,
        }
    }
    Ok(mask)
}

fn tag_bit(tag: u32) -> Result<u32, UnknownAction> {
    // 基线是 9 个 tag。
    if !(1..=9).contains(&tag) {
        return Err(UnknownAction(format!("tag 超出 1..=9：{tag}")));
    }
    Ok(1 << (tag - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_one_based() {
        assert_eq!(Action::parse("view", Some("1")).unwrap(), Action::View(0b1));
        assert_eq!(
            Action::parse("view", Some("9")).unwrap(),
            Action::View(0b1_0000_0000)
        );
    }

    #[test]
    fn tags_support_ranges_and_commas() {
        assert_eq!(
            Action::parse("view", Some("1-3")).unwrap(),
            Action::View(0b111)
        );
        assert_eq!(
            Action::parse("view", Some("1,4")).unwrap(),
            Action::View(0b1001)
        );
    }

    #[test]
    fn out_of_range_tag_is_rejected() {
        assert!(Action::parse("view", Some("10")).is_err());
        assert!(Action::parse("view", None).is_err());
    }

    #[test]
    fn spawn_splits_on_whitespace() {
        assert_eq!(
            Action::parse("spawn", Some("foot --server")).unwrap(),
            Action::Spawn(vec!["foot".into(), "--server".into()])
        );
    }

    #[test]
    fn unknown_action_is_rejected() {
        assert!(Action::parse("explode", None).is_err());
    }

    #[test]
    fn tag_toggling_and_view_prev_parse() {
        assert_eq!(
            Action::parse("toggle_tag", Some("1,3")).unwrap(),
            Action::ToggleTag(0b101)
        );
        assert_eq!(
            Action::parse("toggletag", Some("2")).unwrap(),
            Action::ToggleTag(0b10)
        );
        assert_eq!(Action::parse("view_prev", None).unwrap(), Action::ViewPrev);
        assert_eq!(Action::parse("viewprev", None).unwrap(), Action::ViewPrev);
    }

    #[test]
    fn shift_view_parses_its_step() {
        assert_eq!(
            Action::parse("shift_view", Some("-1")).unwrap(),
            Action::ShiftView(-1)
        );
        assert_eq!(
            Action::parse("shiftview", Some("1")).unwrap(),
            Action::ShiftView(1)
        );
        assert!(Action::parse("shift_view", None).is_err(), "缺参数要报错");
        assert!(
            Action::parse("shift_view", Some("x")).is_err(),
            "非数字要报错"
        );
    }

    #[test]
    fn titlebar_toggle_accepts_both_names() {
        assert_eq!(
            Action::parse("toggle_titlebar", None).unwrap(),
            Action::ToggleTitlebar
        );
        assert_eq!(
            Action::parse("toggletitlebar", None).unwrap(),
            Action::ToggleTitlebar
        );
    }

    #[test]
    fn notification_actions_accept_baseline_names() {
        for (name, action) in [
            ("notifyopen", Action::NotifyOpen),
            ("notifydismiss", Action::NotifyDismiss),
            ("notifydismissall", Action::NotifyDismissAll),
            ("notifynext", Action::NotifyNext),
            ("notifyprev", Action::NotifyPrev),
            ("notifyscroll", Action::NotifyScroll),
            ("notifyactions", Action::NotifyActions),
        ] {
            assert_eq!(Action::parse(name, None).unwrap(), action);
        }
        assert_eq!(
            Action::parse("notify_open", None).unwrap(),
            Action::NotifyOpen
        );
    }

    #[test]
    fn runner_toggle_accepts_baseline_name() {
        assert_eq!(
            Action::parse("runnertoggle", None).unwrap(),
            Action::RunnerToggle
        );
        assert_eq!(
            Action::parse("runner_toggle", None).unwrap(),
            Action::RunnerToggle
        );
    }

    #[test]
    fn close_accepts_baseline_name() {
        assert_eq!(Action::parse("close", None).unwrap(), Action::Close);
        assert_eq!(Action::parse("kill", None).unwrap(), Action::Close);
    }

    #[test]
    fn focus_output_accepts_both_spellings() {
        assert_eq!(
            Action::parse("focus_output", None).unwrap(),
            Action::FocusOutput
        );
        assert_eq!(
            Action::parse("focusoutput", None).unwrap(),
            Action::FocusOutput
        );
        assert_eq!(
            Action::parse("focus_output_next", None).unwrap(),
            Action::FocusOutput
        );
    }

    #[test]
    fn move_output_accepts_both_spellings() {
        for name in [
            "move_output",
            "move_output_next",
            "move_window_to_output",
            "move_window_to_output_next",
        ] {
            assert_eq!(Action::parse(name, None).unwrap(), Action::MoveOutput);
        }
    }
}

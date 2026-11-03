//! StatusNotifierItem 托盘：状态机与 D-Bus 服务（对标基线 `src/systray/*`）。
//!
//! 分工与通知一致：
//!
//! - 这里（[`Snapshot`] / [`MenuItem`]）只放纯数据，不依赖 zbus，可单测；
//! - [`service`] 在独立线程里跑 `org.kde.StatusNotifierWatcher`，管理
//!   item 的属性查询、图标解码与 dbusmenu 菜单，把快照推回 calloop channel；
//! - bar 只负责把快照画成右端的一排图标，弹出菜单由 [`crate::ui::tray`]
//!   叠在窗口之上。
//!
//! 命名照抄基线：规范写的是 `org.freedesktop.*`，但真实客户端（
//! libayatana-appindicator、KStatusNotifierItem）用的是 `org.kde.*`。

mod service;

pub use service::{TrayHandle, start};

use std::sync::Arc;

use crate::ui::icon::Icon;

/// StatusNotifierItem 接口名（基线的 `SNI_IFACE`）。
pub const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
/// 托盘宿主注册的名字（基线 `SNW_NAME`）。
pub const SNI_HOST_NAME: &str = "org.kde.StatusNotifierWatcher";
/// 宿主对象路径（基线 `SNW_OPATH`）。
pub const SNI_HOST_PATH: &str = "/StatusNotifierWatcher";
/// item 的默认对象路径（基线 `SNI_OPATH`）。
pub const SNI_ITEM_PATH: &str = "/StatusNotifierItem";
/// dbusmenu 接口名。
pub const DBUSMENU_IFACE: &str = "com.canonical.dbusmenu";

/// 菜单项标签的字节上限，含勾选前缀与子菜单箭头（基线 `MENU_LABEL_MAX`）。
pub const MENU_LABEL_MAX: usize = 64;
/// 单层最多显示的条目数（基线 `ITEMS_MAX`）。
pub const MENU_ITEMS_MAX: usize = 64;

/// 托盘快照：bar 每次重画都只读它。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// 按注册顺序排列的 item。
    pub items: Vec<ItemView>,
    /// 正在弹出的菜单；`None` 表示没有菜单。
    pub menu: Option<MenuView>,
}

/// 一个托盘 item 的呈现信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ItemView {
    /// D-Bus 上的名字；左键 Activate、右键菜单都用它定位。
    pub busname: String,
    /// `Id` 属性，给没有图标时画首字母用。
    pub appid: String,
    /// 已经解码好的图标；没有时用 `appid` 的首字母回退。
    pub icon: Option<Arc<Icon>>,
}

impl ItemView {
    /// 回退图标要画的字符；`appid` 为空时不画。
    pub fn fallback_letter(&self) -> Option<char> {
        self.appid.chars().find(|c| !c.is_whitespace())
    }
}

/// 当前这一层菜单。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuView {
    pub labels: Vec<String>,
}

/// dbusmenu 的一个节点（纯数据，已按基线规则过滤/格式化）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub has_submenu: bool,
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    /// 把一层节点摊平成可显示的一行行标签，最多 [`MENU_ITEMS_MAX`] 条。
    pub fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .take(MENU_ITEMS_MAX)
            .map(|item| item.label.clone())
            .collect()
    }

    /// 把一层节点包成 [`MenuView`]；空层返回 `None`。
    pub fn view(items: &[MenuItem]) -> Option<MenuView> {
        let labels = Self::labels(items);
        (!labels.is_empty()).then_some(MenuView { labels })
    }
}

/// 按基线 `createmenuitem` 的规则把 dbusmenu 的原始标签变成显示文本。
///
/// - 勾选项前面加 `☐ `/`✓ `，普通项加两个空格占位；
/// - 去掉表示 mnemonics 的下划线；
/// - 有子菜单的追加 ` →`；
/// - 超长标签按字节安全截断到 [`MENU_LABEL_MAX`]。
pub fn format_label(raw: &str, toggle_state: Option<i32>, has_submenu: bool) -> String {
    let prefix = match toggle_state {
        Some(0) => "☐ ",
        Some(1) => "✓ ",
        _ => "  ",
    };
    let suffix = if has_submenu { " →" } else { "" };
    let mut out = String::with_capacity(prefix.len() + raw.len() + suffix.len());
    out.push_str(prefix);
    out.extend(raw.chars().filter(|c| *c != '_'));

    // 预留后缀，按字符边界截断；基线按原始字节长度判超限，这里给同样的上限。
    let budget = MENU_LABEL_MAX
        .saturating_sub(prefix.len() + suffix.len())
        .min(out.len().saturating_sub(prefix.len()));
    if out.len() - prefix.len() > budget {
        let mut end = prefix.len() + budget;
        while end > prefix.len() && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i32, label: &str, children: Vec<MenuItem>) -> MenuItem {
        MenuItem {
            id,
            label: label.into(),
            has_submenu: !children.is_empty(),
            children,
        }
    }

    #[test]
    fn plain_label_gets_alignment_prefix() {
        assert_eq!(format_label("Open", None, false), "  Open");
    }

    #[test]
    fn checkmark_uses_box_and_tick() {
        assert_eq!(format_label("Show", Some(0), false), "☐ Show");
        assert_eq!(format_label("Show", Some(1), false), "✓ Show");
        assert_eq!(format_label("Show", Some(7), false), "  Show");
    }

    #[test]
    fn mnemonics_are_stripped() {
        assert_eq!(format_label("_Open _File", None, false), "  Open File");
    }

    #[test]
    fn submenu_gets_arrow() {
        assert_eq!(format_label("More", None, true), "  More →");
        assert_eq!(format_label("More", Some(1), true), "✓ More →");
    }

    #[test]
    fn long_labels_are_truncated_on_char_boundary() {
        let raw = "很长的菜单标签".repeat(20);
        let label = format_label(&raw, None, true);
        assert!(label.len() <= MENU_LABEL_MAX, "{}", label.len());
        assert!(label.ends_with(" →"));
        // 截断点必须仍然可以正常作为 UTF-8 渲染。
        assert!(std::str::from_utf8(label.as_bytes()).is_ok());
    }

    #[test]
    fn labels_are_capped_per_level() {
        let items: Vec<MenuItem> = (0..100).map(|i| item(i, "x", Vec::new())).collect();
        assert_eq!(MenuItem::labels(&items).len(), MENU_ITEMS_MAX);
    }

    #[test]
    fn empty_level_has_no_view() {
        assert_eq!(MenuItem::view(&[]), None);
        let one = [item(1, &format_label("A", None, false), Vec::new())];
        assert_eq!(
            MenuItem::view(&one),
            Some(MenuView {
                labels: vec!["  A".into()]
            })
        );
    }

    #[test]
    fn fallback_letter_skips_whitespace() {
        let view = ItemView {
            busname: "org.example.App".into(),
            appid: "  Steam".into(),
            icon: None,
        };
        assert_eq!(view.fallback_letter(), Some('S'));
        let empty = ItemView {
            appid: String::new(),
            ..view
        };
        assert_eq!(empty.fallback_letter(), None);
    }
}

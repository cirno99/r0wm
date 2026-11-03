//! 布局层：纯数据、纯函数。
//!
//! 铁律（`docs/adr/0002`、共识「单 crate + 依赖隔离」）：
//! **本模块及其子模块不 import Smithay**。输入是输出矩形、bar 占用与行状态，输出是
//! 目标矩形（`CONTEXT.md` 的「目标矩形」）。这样整层可以脱离合成器做单元测试。

pub mod bsp;
pub mod scroll;
pub mod workspace;

pub use workspace::{Workspace, WorkspaceSet};

/// 屏幕矩形，逻辑像素。y 向下为正。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// 两个矩形是否有交集。贴边不算（与 Smithay 的 `overlaps` 一致）。
    pub fn overlaps(self, other: Rect) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// 交互缩放拖动的边。布局层不依赖 Smithay，所以用自己的小结构。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// 一个窗口标题条的目标矩形（平铺层）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleSlice {
    pub id: u64,
    pub rect: Rect,
    /// 属于多窗口的 tab 组。
    pub tab: bool,
    /// 组内第一个窗口。
    pub first_tab: bool,
}

/// 比例：相对所在行/列的**可用空间**（`CONTEXT.md`「比例」），不是相对屏幕。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Fraction(f64);

impl Fraction {
    pub const MIN: f64 = 0.05;
    pub const MAX: f64 = 1.0;

    /// 越界会被夹到 `[MIN, MAX]`。
    pub fn new(value: f64) -> Self {
        Self(value.clamp(Self::MIN, Self::MAX))
    }

    pub fn value(self) -> f64 {
        self.0
    }

    /// 比例落到一段可用长度上的实际像素。
    pub fn pixels(self, extent: i32) -> i32 {
        (extent as f64 * self.0).round() as i32
    }
}

impl Default for Fraction {
    fn default() -> Self {
        Self(1.0)
    }
}

/// 方向：列/行/几何邻居导航共用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// 三个可切换的平铺布局（`docs/adr/0003` 修订：`bsp` 是默认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TiledLayout {
    #[default]
    Bsp,
    Scroll,
    /// scroll 引擎在「单列、宽 1.0、高 1.0」下的退化实例。
    Tabbed,
}

impl TiledLayout {
    /// 配置与 IPC 里使用的稳定名字（`set_layout` 的对象）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Bsp => "bsp",
            Self::Scroll => "scroll",
            Self::Tabbed => "tabbed",
        }
    }
}

impl std::fmt::Display for TiledLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// bar 从输出里挤掉的区域（`CONTEXT.md`「bar」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarSpace {
    pub height: i32,
    pub at_top: bool,
}

impl Default for BarSpace {
    fn default() -> Self {
        Self {
            height: 0,
            at_top: true,
        }
    }
}

/// 扣掉 bar 之后的工作区（`CONTEXT.md`「行可用高」）。
pub fn work_area(output: Rect, bar: BarSpace) -> Rect {
    let bar_height = bar.height.clamp(0, output.height);
    if bar.at_top {
        Rect {
            y: output.y + bar_height,
            height: output.height - bar_height,
            ..output
        }
    } else {
        Rect {
            height: output.height - bar_height,
            ..output
        }
    }
}

/// 位掩码里最低的那个 tag（单 bit），例如 `0b1010` → `0b0010`。
///
/// M2 的窗口归属只取一个 tag；多 tag 归属等 `ToggleTag` 动作（M3）。
pub fn lowest_tag(mask: u32) -> Option<u32> {
    (mask != 0).then(|| mask & mask.wrapping_neg())
}

/// 位掩码里的 tag（单 bit），升序。
pub fn tags_in(mask: u32) -> impl Iterator<Item = u32> {
    (0..32)
        .map(|i| 1u32 << i)
        .filter(move |bit| mask & bit != 0)
}

/// 多个可见行（多个 tag 同时可见）在垂直方向等分工作区（`docs/adr/0004`）。
///
/// 余数分配给靠前的行，保证总高度不丢像素。
pub fn row_areas(work: Rect, rows: usize) -> Vec<Rect> {
    if rows == 0 {
        return Vec::new();
    }
    let rows = rows as i32;
    let base = work.height / rows;
    let leftover = work.height - base * rows;

    (0..rows)
        .map(|i| {
            let extra = i32::from(i < leftover);
            Rect {
                y: work.y + i * base + i.min(leftover),
                height: base + extra,
                ..work
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn bar_at_top_pushes_down() {
        let work = work_area(
            rect(),
            BarSpace {
                height: 30,
                at_top: true,
            },
        );
        assert_eq!(work.y, 30);
        assert_eq!(work.height, 1050);
    }

    #[test]
    fn bar_at_bottom_only_shrinks_height() {
        let work = work_area(
            rect(),
            BarSpace {
                height: 30,
                at_top: false,
            },
        );
        assert_eq!(work.y, 0);
        assert_eq!(work.height, 1050);
    }

    #[test]
    fn rows_split_without_losing_pixels() {
        let work = Rect {
            height: 100,
            ..rect()
        };
        let areas = row_areas(work, 3);
        assert_eq!(areas[0].height, 34);
        assert_eq!(areas[1].height, 33);
        assert_eq!(areas[2].height, 33);
        assert_eq!(areas.iter().map(|r| r.height).sum::<i32>(), 100);
        assert_eq!(areas[1].y, 34);
        assert_eq!(areas[2].y, 67);
    }

    #[test]
    fn fraction_is_clamped() {
        assert_eq!(Fraction::new(2.0).value(), 1.0);
        assert_eq!(Fraction::new(0.0).value(), Fraction::MIN);
        assert_eq!(Fraction::new(0.5).pixels(1920), 960);
    }

    #[test]
    fn layout_names_are_stable() {
        assert_eq!(TiledLayout::Bsp.name(), "bsp");
        assert_eq!(TiledLayout::Scroll.to_string(), "scroll");
        assert_eq!(TiledLayout::Tabbed.name(), "tabbed");
    }

    #[test]
    fn rect_overlap_excludes_touching_edges() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let partial = Rect {
            x: 99,
            y: 20,
            width: 50,
            height: 20,
        };
        let right_of_area = Rect {
            x: 100,
            y: 20,
            width: 50,
            height: 20,
        };
        let fully_inside = Rect {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        };
        assert!(area.overlaps(partial));
        assert!(area.overlaps(fully_inside));
        assert!(!area.overlaps(right_of_area));
    }
}

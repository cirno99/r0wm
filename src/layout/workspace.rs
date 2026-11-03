//! 一个 tag 的布局状态，以及「多行」的 tag 集合。
//!
//! ADR 0004：一个 tag = 一行；多个可见 tag 垂直等分工作区。ADR 0003：三套布局
//! 共享同一份窗口顺序，切换布局只重建几何结构、不改变焦点链。
//!
//! 本模块仍然是纯数据：输入工作区矩形，输出目标矩形。窗口句柄留在 `shell`。

use std::collections::BTreeMap;

use super::{
    Direction, Fraction, Rect, TiledLayout, TitleSlice, bsp, lowest_tag, row_areas, scroll::Row,
    tags_in,
};

/// 单个 tag 的布局状态。
///
/// `tree` 与 `row` 只有当前布局对应的那个是有效的；切布局时按 `order()` 重建目标
/// 结构，因此脏掉的那个不会被读到。
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub layout: TiledLayout,
    /// scroll 新列的默认宽度比例。
    pub width: Fraction,
    tree: bsp::Tree,
    row: Row,
}

impl Workspace {
    pub fn new(layout: TiledLayout, width: Fraction) -> Self {
        Self {
            layout,
            width,
            tree: bsp::Tree::default(),
            row: Row::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.order().is_empty()
    }

    /// 当前布局下的窗口顺序。bsp 是叶子深度优先，scroll/tabbed 是列序 + 列内 tab 序。
    pub fn order(&self) -> Vec<u64> {
        match self.layout {
            TiledLayout::Bsp => self.tree.leaves(),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.all_ids(),
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.order().contains(&id)
    }

    /// 新窗口进入布局：bsp 在焦点叶子处分屏，scroll 在焦点列右侧建新列，
    /// tabbed 把新窗口变成当前列的 tab。
    pub fn insert(&mut self, focused: Option<u64>, id: u64) {
        match self.layout {
            TiledLayout::Bsp => self.tree.insert(focused, id),
            TiledLayout::Scroll => {
                let index = focused
                    .and_then(|focused| self.row.column_of(focused))
                    .map_or(self.row.columns.len(), |index| index + 1);
                self.row.insert_column(index, id, self.width);
            }
            TiledLayout::Tabbed => {
                self.row.ensure_tabbed();
                self.row.insert_tab(0, id);
            }
        }
    }

    pub fn remove(&mut self, id: u64) -> bool {
        match self.layout {
            TiledLayout::Bsp => self.tree.remove(id),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.remove(id),
        }
    }

    /// 交互缩放平铺窗口：bsp 改分割比例，scroll 改列宽。
    pub fn resize(
        &mut self,
        id: u64,
        area: Rect,
        edges: super::ResizeEdges,
        dx: f64,
        dy: f64,
    ) -> bool {
        match self.layout {
            TiledLayout::Bsp => self.tree.resize(id, area, edges, dx, dy),
            TiledLayout::Scroll => self.row.resize(id, area, edges, dx),
            TiledLayout::Tabbed => false,
        }
    }

    /// scroll 布局下把窗口并入相邻列；其他布局没有「列」的概念。
    pub fn consume(&mut self, id: u64, direction: Direction) -> bool {
        if self.layout != TiledLayout::Scroll {
            return false;
        }
        self.row.consume(id, direction).is_some()
    }

    /// scroll 布局下把窗口从当前列拆到右侧新列。
    pub fn expel(&mut self, id: u64) -> bool {
        if self.layout != TiledLayout::Scroll {
            return false;
        }
        self.row.expel(id).is_some()
    }

    /// 交换两个窗口的布局位置（i3 式拖动平铺窗口）。
    pub fn swap(&mut self, a: u64, b: u64) -> bool {
        match self.layout {
            TiledLayout::Bsp => self.tree.swap(a, b),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.swap(a, b),
        }
    }

    /// 切换布局：保留窗口顺序，重建目标布局的数据结构。
    pub fn set_layout(&mut self, layout: TiledLayout) {
        if self.layout == layout {
            return;
        }
        let order = self.order();
        self.layout = layout;
        match layout {
            TiledLayout::Bsp => self.tree = bsp::Tree::from_order(&order),
            TiledLayout::Scroll => self.row = Row::from_ids(&order, self.width),
            TiledLayout::Tabbed => self.row = Row::tabbed(&order),
        }
    }

    /// 每个可见窗口的目标矩形。
    pub fn target_rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        match self.layout {
            TiledLayout::Bsp => self.tree.target_rects(area),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.target_rects(area),
        }
    }

    /// 每个窗口标题条的位置：bsp 每个叶子一条，scroll/tabbed 每组 tab 平分一条。
    pub fn title_slices(&self, area: Rect) -> Vec<TitleSlice> {
        match self.layout {
            TiledLayout::Bsp => self
                .tree
                .target_rects(area)
                .into_iter()
                .map(|(id, rect)| TitleSlice {
                    id,
                    rect,
                    tab: false,
                    first_tab: true,
                })
                .collect(),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.title_slices(area),
        }
    }

    /// 焦点列的栈顶；没有窗口时为 `None`。bsp 用第一个叶子做兜底。
    pub fn top_window(&self) -> Option<u64> {
        match self.layout {
            TiledLayout::Bsp => self.tree.leaves().first().copied(),
            TiledLayout::Scroll | TiledLayout::Tabbed => self.row.top_window(),
        }
    }

    /// 把焦点同步到某个窗口：scroll/tabbed 让它成为所在列的栈顶并保证可见。
    pub fn focus_window(&mut self, id: u64, area: Rect) {
        if self.layout == TiledLayout::Bsp {
            return;
        }
        let Some(column) = self.row.column_of(id) else {
            return;
        };
        self.row.focus = column;
        if let Some(column) = self.row.columns.get_mut(column) {
            column.set_top(id);
        }
        self.row.scroll_to_focus(area);
    }

    /// 行列方向导航。左右在 scroll 里换列、在 bsp 里找几何邻居；
    /// 上下由多行（tag）负责，单行布局里返回 `None`。
    pub fn focus_direction(
        &mut self,
        focused: Option<u64>,
        area: Rect,
        direction: Direction,
    ) -> Option<u64> {
        match self.layout {
            TiledLayout::Bsp => {
                let rects = self.tree.target_rects(area);
                let current = focused
                    .filter(|id| rects.iter().any(|(leaf, _)| leaf == id))
                    .or_else(|| rects.first().map(|(leaf, _)| *leaf))?;
                bsp::neighbor(&rects, current, direction)
            }
            TiledLayout::Scroll => {
                if matches!(direction, Direction::Up | Direction::Down) {
                    return None;
                }
                if let Some(focused) = focused
                    && let Some(column) = self.row.column_of(focused)
                {
                    self.row.focus = column;
                }
                let step = match direction {
                    Direction::Left => -1,
                    Direction::Right => 1,
                    Direction::Up | Direction::Down => return None,
                };
                if !self.row.focus_column(step) {
                    return None;
                }
                self.row.scroll_to_focus(area);
                self.row.top_window()
            }
            // tabbed 是单列布局，左右没有可去的地方；列内切换用 `cycle_tab`。
            TiledLayout::Tabbed => None,
        }
    }

    /// 列内 tab 循环；bsp 退化成「按深度优先顺序循环窗口」。
    pub fn cycle_tab(&mut self, focused: Option<u64>, step: isize) -> Option<u64> {
        match self.layout {
            TiledLayout::Bsp => {
                let leaves = self.tree.leaves();
                let index = focused
                    .and_then(|id| leaves.iter().position(|leaf| *leaf == id))
                    .unwrap_or(0);
                let len = leaves.len() as isize;
                if len == 0 {
                    return None;
                }
                let next = (index as isize + step).rem_euclid(len) as usize;
                Some(leaves[next])
            }
            TiledLayout::Scroll | TiledLayout::Tabbed => {
                if let Some(focused) = focused
                    && let Some(column) = self.row.column_of(focused)
                {
                    self.row.focus = column;
                }
                self.row.cycle_tab(step)
            }
        }
    }

    /// 按行偏移规则把焦点列滚进视野。
    pub fn scroll_to_focus(&mut self, area: Rect) {
        if matches!(self.layout, TiledLayout::Scroll | TiledLayout::Tabbed) {
            self.row.scroll_to_focus(area);
        }
    }
}

/// 所有 tag 的布局，以及可见性/当前行状态。
///
/// 单独抽出来是为了能脱离 `Window` 单测；`Shell` 只负责窗口句柄与焦点栈。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSet {
    default_layout: TiledLayout,
    default_width: Fraction,
    workspaces: BTreeMap<u32, Workspace>,
    visible_tags: u32,
    /// 上一套可见 tag 集合；`view_prev` 与它交换（基线 `tagset[2]`）。
    previous_tags: u32,
    focused_tag: u32,
}

impl WorkspaceSet {
    pub fn new(default_layout: TiledLayout, default_width: Fraction) -> Self {
        Self {
            default_layout,
            default_width,
            workspaces: BTreeMap::new(),
            visible_tags: 1,
            previous_tags: 1,
            focused_tag: 1,
        }
    }

    pub fn visible_tags(&self) -> u32 {
        self.visible_tags
    }

    /// `view`：替换可见集合。空集合会被忽略。
    pub fn set_visible_tags(&mut self, mask: u32) {
        if mask == 0 || mask == self.visible_tags {
            return;
        }
        self.previous_tags = self.visible_tags;
        self.visible_tags = mask;
        if self.focused_tag & mask == 0 {
            self.focused_tag = lowest_tag(mask).expect("mask 非零");
        }
    }

    /// `view_prev`：与上一套可见集合交换。
    pub fn swap_visible_tags(&mut self) -> bool {
        if self.previous_tags == 0 || self.previous_tags == self.visible_tags {
            return false;
        }
        std::mem::swap(&mut self.visible_tags, &mut self.previous_tags);
        if self.focused_tag & self.visible_tags == 0 {
            self.focused_tag = lowest_tag(self.visible_tags).expect("可见集合非空");
        }
        true
    }

    /// `toggleview`：异或；结果为空时忽略。
    pub fn toggle_visible_tags(&mut self, mask: u32) {
        let next = self.visible_tags ^ mask;
        if next != 0 {
            self.set_visible_tags(next);
        }
    }

    pub fn focused_tag(&self) -> u32 {
        self.focused_tag
    }

    /// 只有可见的 tag 才能成为当前行。
    pub fn set_focused_tag(&mut self, tag: u32) {
        if tag & self.visible_tags != 0 {
            self.focused_tag = tag;
        }
    }

    /// 可见 tag（单 bit）升序，也就是屏幕上的行顺序。
    pub fn visible_tag_list(&self) -> Vec<u32> {
        tags_in(self.visible_tags).collect()
    }

    pub fn workspace(&self, tag: u32) -> Option<&Workspace> {
        self.workspaces.get(&lowest_tag(tag)?)
    }

    /// 窗口当前挂在哪个 tag 的布局里（成员的「家」）；不在任何布局里时 `None`。
    pub fn home_of(&self, id: u64) -> Option<u32> {
        self.workspaces
            .iter()
            .find(|(_, workspace)| workspace.contains(id))
            .map(|(tag, _)| *tag)
    }

    /// 某个 tag 当前使用的布局；还没有窗口的 tag 报告默认布局。
    pub fn layout_of(&self, tag: u32) -> TiledLayout {
        self.workspace(tag)
            .map(|workspace| workspace.layout)
            .unwrap_or(self.default_layout)
    }

    pub fn workspace_mut(&mut self, tag: u32) -> Option<&mut Workspace> {
        let tag = lowest_tag(tag)?;
        Some(
            self.workspaces
                .entry(tag)
                .or_insert_with(|| Workspace::new(self.default_layout, self.default_width)),
        )
    }

    pub fn insert(&mut self, tag: u32, focused: Option<u64>, id: u64) {
        if let Some(workspace) = self.workspace_mut(tag) {
            workspace.insert(focused, id);
        }
    }

    pub fn remove(&mut self, tag: u32, id: u64) {
        if let Some(workspace) = self.workspace_mut(tag) {
            workspace.remove(id);
        }
    }

    /// scroll 下把窗口并入相邻列。
    pub fn consume(&mut self, id: u64, direction: Direction) -> bool {
        let Some(home) = self.home_of(id) else {
            return false;
        };
        self.workspace_mut(home)
            .is_some_and(|workspace| workspace.consume(id, direction))
    }

    /// scroll 下把窗口从当前列拆到新列。
    pub fn expel(&mut self, id: u64) -> bool {
        let Some(home) = self.home_of(id) else {
            return false;
        };
        self.workspace_mut(home)
            .is_some_and(|workspace| workspace.expel(id))
    }

    /// 平铺窗口的交互缩放；`work` 是输出的工作区，内部按可见行再分一次。
    pub fn resize(
        &mut self,
        id: u64,
        work: Rect,
        edges: super::ResizeEdges,
        dx: f64,
        dy: f64,
    ) -> bool {
        let Some(home) = self.home_of(id) else {
            return false;
        };
        let visible = self.visible_tag_list();
        let row = visible
            .iter()
            .position(|tag| *tag == home)
            .unwrap_or_else(|| {
                visible
                    .iter()
                    .position(|tag| *tag == self.focused_tag)
                    .unwrap_or(0)
            });
        let area = row_areas(work, visible.len())
            .get(row)
            .copied()
            .unwrap_or(work);
        self.workspace_mut(home)
            .is_some_and(|workspace| workspace.resize(id, area, edges, dx, dy))
    }

    /// 交换同一行内两个窗口的位置；跨行不交换（基线也只交换同输出的格子）。
    pub fn swap(&mut self, a: u64, b: u64) -> bool {
        let Some(home) = self.home_of(a) else {
            return false;
        };
        if self.home_of(b) != Some(home) {
            return false;
        }
        self.workspace_mut(home)
            .is_some_and(|workspace| workspace.swap(a, b))
    }

    pub fn set_layout(&mut self, tag: u32, layout: TiledLayout) {
        if let Some(workspace) = self.workspace_mut(tag) {
            workspace.set_layout(layout);
        }
    }

    /// 只影响之后新建的 tag 布局；已存在的 tag 保留自己的选择（ADR 0003）。
    pub fn set_default_layout(&mut self, layout: TiledLayout) {
        self.default_layout = layout;
    }

    /// 某个 tag 在屏幕上的行区域。
    pub fn tag_area(&self, tag: u32, work: Rect) -> Option<Rect> {
        let tags = self.visible_tag_list();
        let index = tags.iter().position(|visible| *visible == tag)?;
        row_areas(work, tags.len()).get(index).copied()
    }

    /// 可见 tag 与各自的行区域。
    pub fn tag_areas(&self, work: Rect) -> Vec<(u32, Rect)> {
        let tags = self.visible_tag_list();
        row_areas(work, tags.len())
            .into_iter()
            .zip(tags)
            .map(|(area, tag)| (tag, area))
            .collect()
    }

    /// 重新计算滚动偏移：在产出目标矩形之前调用。
    pub fn relayout(&mut self, work: Rect) {
        for (tag, area) in self.tag_areas(work) {
            if let Some(workspace) = self.workspace_mut(tag) {
                workspace.scroll_to_focus(area);
            }
        }
    }

    /// 所有平铺窗口的目标矩形（多行按 tag 分批产出）。
    pub fn arrange_targets(&self, work: Rect) -> Vec<(u64, Rect)> {
        self.tag_areas(work)
            .into_iter()
            .filter_map(|(tag, area)| self.workspace(tag).map(|w| w.target_rects(area)))
            .flatten()
            .collect()
    }

    /// 所有平铺窗口的标题条位置（多行按 tag 分批产出）。
    pub fn title_slices(&self, work: Rect) -> Vec<TitleSlice> {
        self.tag_areas(work)
            .into_iter()
            .filter_map(|(tag, area)| self.workspace(tag).map(|w| w.title_slices(area)))
            .flatten()
            .collect()
    }

    /// 当前行内的左右导航。
    pub fn focus_column(
        &mut self,
        direction: Direction,
        focused: Option<u64>,
        work: Rect,
    ) -> Option<u64> {
        let tag = self.focused_tag;
        let area = self.tag_area(tag, work)?;
        self.workspace_mut(tag)?
            .focus_direction(focused, area, direction)
    }

    /// 当前列的 tab 循环。
    pub fn cycle_tab(&mut self, focused: Option<u64>, step: isize) -> Option<u64> {
        let tag = self.focused_tag;
        self.workspace_mut(tag)?.cycle_tab(focused, step)
    }

    /// 焦点落到某个窗口：同步当前行与滚动位置。
    pub fn note_focus(&mut self, id: u64, work: Rect) {
        let Some(tag) = self.home_of(id) else {
            return;
        };
        if tag & self.visible_tags == 0 {
            return;
        }
        self.focused_tag = tag;
        if let Some(area) = self.tag_area(tag, work)
            && let Some(workspace) = self.workspace_mut(tag)
        {
            workspace.focus_window(id, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width() -> Fraction {
        Fraction::new(0.5)
    }

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
        }
    }

    #[test]
    fn bsp_workspace_tiles_three_windows_without_overlap() {
        let mut workspace = Workspace::new(TiledLayout::Bsp, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);

        let rects = workspace.target_rects(area());
        assert_eq!(rects.len(), 3);
        let sum: i32 = rects.iter().map(|(_, rect)| rect.width * rect.height).sum();
        assert_eq!(sum, 1000 * 800);
    }

    #[test]
    fn scroll_workspace_creates_a_column_per_window() {
        let mut workspace = Workspace::new(TiledLayout::Scroll, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);

        assert_eq!(workspace.order(), vec![1, 2]);
        // 第二列在 500px 处，宽度 500；焦点列滚动后 offset 不变化（刚好放下）。
        let rects = workspace.target_rects(area());
        assert_eq!(rects[1].1.x, 500);
        assert_eq!(rects[1].1.width, 500);
    }

    #[test]
    fn bsp_focus_direction_moves_between_vertical_neighbors() {
        let mut workspace = Workspace::new(TiledLayout::Bsp, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);

        assert_eq!(
            workspace.focus_direction(Some(3), area(), Direction::Up),
            Some(2)
        );
        assert_eq!(
            workspace.focus_direction(Some(2), area(), Direction::Down),
            Some(3)
        );
    }

    #[test]
    fn scroll_focus_right_scrolls_the_row() {
        let mut workspace = Workspace::new(TiledLayout::Scroll, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);

        let focused = workspace.focus_direction(Some(3), area(), Direction::Right);
        assert_eq!(focused, None, "已经在最右列");

        let focused = workspace.focus_direction(Some(3), area(), Direction::Left);
        assert_eq!(focused, Some(2));
        assert_eq!(workspace.target_rects(area())[1].1.x, 500);
    }

    #[test]
    fn tabbed_workspace_shows_only_the_top_tab() {
        let mut workspace = Workspace::new(TiledLayout::Tabbed, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);

        let rects = workspace.target_rects(area());
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, 3);
        assert_eq!(
            rects[0].1,
            Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 800
            }
        );
    }

    #[test]
    fn tab_next_cycles_and_focuses_tabs() {
        let mut workspace = Workspace::new(TiledLayout::Tabbed, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);

        assert_eq!(workspace.cycle_tab(Some(3), 1), Some(1));
        assert_eq!(workspace.cycle_tab(Some(1), -1), Some(3));
    }

    #[test]
    fn focus_window_brings_tab_to_top_and_scrolls() {
        let mut workspace = Workspace::new(TiledLayout::Tabbed, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);

        workspace.focus_window(1, area());
        assert_eq!(workspace.target_rects(area())[0].0, 1);
    }

    #[test]
    fn scroll_remove_fixes_focus_index() {
        let mut workspace = Workspace::new(TiledLayout::Scroll, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);
        // 焦点在第 2 列（id=3）
        assert!(workspace.remove(2));
        assert_eq!(workspace.order(), vec![1, 3]);
        assert!(workspace.remove(1));
        assert_eq!(workspace.order(), vec![3]);
        assert!(workspace.remove(3));
        assert!(workspace.is_empty());
    }

    #[test]
    fn switching_layouts_preserves_order() {
        let mut workspace = Workspace::new(TiledLayout::Bsp, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(Some(2), 3);
        let order = workspace.order();

        workspace.set_layout(TiledLayout::Scroll);
        assert_eq!(workspace.order(), order);
        workspace.set_layout(TiledLayout::Tabbed);
        assert_eq!(workspace.order(), order);
        workspace.set_layout(TiledLayout::Bsp);
        assert_eq!(workspace.order(), order);
    }

    #[test]
    fn insert_without_focus_uses_last_leaf_in_bsp() {
        let mut workspace = Workspace::new(TiledLayout::Bsp, width());
        workspace.insert(None, 1);
        workspace.insert(Some(1), 2);
        workspace.insert(None, 3);
        assert_eq!(workspace.order(), vec![1, 2, 3]);
    }

    #[test]
    fn multiple_visible_tags_split_the_work_area() {
        let mut set = WorkspaceSet::new(TiledLayout::Scroll, width());
        set.insert(0b1, None, 1);
        set.insert(0b10, None, 2);
        set.set_visible_tags(0b11);

        let targets = set.arrange_targets(area());
        assert_eq!(targets.len(), 2);

        let first = targets.iter().find(|(id, _)| *id == 1).unwrap().1;
        let second = targets.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(first.y, 0);
        assert_eq!(first.height, 400);
        assert_eq!(second.y, 400);
        assert_eq!(second.height, 400);
    }

    #[test]
    fn view_and_toggle_view_keep_a_nonzero_visible_set() {
        let mut set = WorkspaceSet::new(TiledLayout::Bsp, width());
        set.set_visible_tags(0b100);
        assert_eq!(set.visible_tags(), 0b100);
        assert_eq!(set.focused_tag(), 0b100);

        set.toggle_visible_tags(0b100);
        assert_eq!(set.visible_tags(), 0b100, "异或为空时忽略");

        set.toggle_visible_tags(0b11);
        assert_eq!(set.visible_tags(), 0b111);
        assert_eq!(set.focused_tag(), 0b100, "当前行仍然可见");
    }

    #[test]
    fn swapping_stays_inside_a_row() {
        let mut set = WorkspaceSet::new(TiledLayout::Bsp, width());
        set.insert(0b1, None, 1);
        set.insert(0b1, Some(1), 2);
        set.insert(0b10, None, 3);

        assert!(set.swap(1, 2));
        assert_eq!(set.workspace(0b1).unwrap().order(), vec![2, 1]);
        assert!(!set.swap(1, 3), "跨行不交换");
    }

    #[test]
    fn view_prev_swaps_tag_sets() {
        let mut set = WorkspaceSet::new(TiledLayout::Bsp, width());
        set.set_visible_tags(0b100);
        set.set_visible_tags(0b1000);

        assert_eq!(set.visible_tags(), 0b1000);
        assert!(set.swap_visible_tags());
        assert_eq!(set.visible_tags(), 0b100);
        assert!(set.swap_visible_tags());
        assert_eq!(set.visible_tags(), 0b1000);
    }

    #[test]
    fn home_of_reports_layout_membership() {
        let mut set = WorkspaceSet::new(TiledLayout::Bsp, width());
        set.insert(0b10, None, 7);

        assert_eq!(set.home_of(7), Some(0b10));
        assert_eq!(set.home_of(8), None);
    }

    #[test]
    fn workspaces_are_independent_per_tag() {
        let mut set = WorkspaceSet::new(TiledLayout::Scroll, width());
        set.insert(0b1, None, 1);
        set.remove(0b1, 1);
        set.insert(0b10, None, 1);

        assert!(set.workspace(0b1).unwrap().is_empty());
        assert_eq!(set.workspace(0b10).unwrap().order(), vec![1]);
    }

    #[test]
    fn focus_column_stays_inside_the_focused_row() {
        let mut set = WorkspaceSet::new(TiledLayout::Scroll, width());
        set.insert(0b1, None, 1);
        set.insert(0b1, Some(1), 2);
        set.note_focus(1, area());

        assert_eq!(set.focus_column(Direction::Right, Some(1), area()), Some(2));
        assert_eq!(
            set.focus_column(Direction::Right, Some(2), area()),
            None,
            "右侧已到边缘"
        );
    }
}

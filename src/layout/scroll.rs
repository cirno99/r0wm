//! scroll 布局：行 / 列 / tab 组 / 行偏移。
//!
//! 语义来自 `docs/adr/0004`（一个 tag = 一行）与 `docs/adr/0005`
//! （列内就是 tab 组，只有水平一条滚动轴）。滚动规则见 `CONTEXT.md`
//! 「行偏移」：最小滚动、贴边对齐。

use super::{Direction, Fraction, Rect, TitleSlice};

/// 一列：宽度比例 + 一个 tab 组。
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub width: Fraction,
    /// tab 组内的窗口，顺序即焦点顺序。
    pub tabs: Vec<u64>,
    /// 栈顶在 `tabs` 中的下标。
    pub top: usize,
}

impl Column {
    pub fn new(width: Fraction, first: u64) -> Self {
        Self {
            width,
            tabs: vec![first],
            top: 0,
        }
    }

    /// 栈顶窗口——它是这一列唯一被绘制的窗口（ADR 0005）。
    pub fn top_window(&self) -> Option<u64> {
        self.tabs.get(self.top).copied()
    }

    /// 让某个 tab 成为栈顶。
    pub fn set_top(&mut self, id: u64) -> bool {
        match self.tabs.iter().position(|tab| *tab == id) {
            Some(index) => {
                self.top = index;
                true
            }
            None => false,
        }
    }

    /// 循环切换栈顶，`step` 为 1 表示下一个 tab。
    pub fn cycle_top(&mut self, step: isize) -> Option<u64> {
        if self.tabs.is_empty() {
            return None;
        }
        let len = self.tabs.len() as isize;
        self.top = (self.top as isize + step).rem_euclid(len) as usize;
        self.top_window()
    }
}

/// 一行：一个 tag 在某个输出上的滚动带。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub columns: Vec<Column>,
    /// 行偏移：所有列的 x 都要加上它（`CONTEXT.md`「行偏移」）。
    pub offset: i32,
    /// 焦点列下标。
    pub focus: usize,
}

impl Row {
    /// 每个 id 一列，列宽相同。
    pub fn from_ids(ids: &[u64], width: Fraction) -> Self {
        Self {
            columns: ids.iter().map(|id| Column::new(width, *id)).collect(),
            offset: 0,
            focus: 0,
        }
    }

    /// tabbed：只有一个整宽列，所有 id 都是这一列的 tab。
    pub fn tabbed(ids: &[u64]) -> Self {
        if ids.is_empty() {
            return Self::default();
        }
        Self {
            columns: vec![Column {
                width: Fraction::new(1.0),
                tabs: ids.to_vec(),
                top: 0,
            }],
            offset: 0,
            focus: 0,
        }
    }

    /// tabbed 布局用：保证只有一个整宽列。
    pub fn ensure_tabbed(&mut self) {
        if self.columns.is_empty() {
            self.columns.push(Column {
                width: Fraction::new(1.0),
                tabs: Vec::new(),
                top: 0,
            });
        }
        self.focus = 0;
    }

    /// 所有窗口 id：列顺序，列内按 tab 顺序（不按栈顶重排）。
    pub fn all_ids(&self) -> Vec<u64> {
        self.columns
            .iter()
            .flat_map(|column| column.tabs.iter().copied())
            .collect()
    }

    /// 窗口所在的列。
    pub fn column_of(&self, id: u64) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.tabs.contains(&id))
    }

    /// 拖动列边缘时调整列宽。scroll 的列是整行高，纵向缩放无意义。
    pub fn resize(&mut self, id: u64, area: Rect, edges: super::ResizeEdges, dx: f64) -> bool {
        if area.width <= 0 || (!edges.left && !edges.right) {
            return false;
        }
        let Some(column) = self.column_of(id) else {
            return false;
        };
        let change = if edges.right { dx } else { -dx } / area.width as f64;
        let width = self.columns[column].width;
        self.columns[column].width = Fraction::new(width.value() + change);
        true
    }

    /// 交换两个窗口所在的格子（列内 tab 位置或列位置），
    /// 用于 i3 式的拖动平铺窗口。
    pub fn swap(&mut self, a: u64, b: u64) -> bool {
        let Some((ca, ta)) = self.position_of(a) else {
            return false;
        };
        let Some((cb, tb)) = self.position_of(b) else {
            return false;
        };
        if (ca, ta) == (cb, tb) {
            return false;
        }
        self.columns[ca].tabs[ta] = b;
        self.columns[cb].tabs[tb] = a;
        true
    }

    fn position_of(&self, id: u64) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(ci, column)| {
            column
                .tabs
                .iter()
                .position(|tab| *tab == id)
                .map(|ti| (ci, ti))
        })
    }

    /// 把新列插到 `index`（越界就追加），并让它成为焦点列。
    pub fn insert_column(&mut self, index: usize, id: u64, width: Fraction) -> usize {
        let index = index.min(self.columns.len());
        self.columns.insert(index, Column::new(width, id));
        self.focus = index;
        index
    }

    /// 在某一列追加 tab，并把它设为栈顶。
    pub fn insert_tab(&mut self, column: usize, id: u64) -> bool {
        let Some(column) = self.columns.get_mut(column) else {
            return false;
        };
        column.tabs.push(id);
        column.top = column.tabs.len() - 1;
        true
    }

    /// 删掉一个窗口；列空了就连列一起删。返回是否真的删到了。
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(ci) = self.column_of(id) else {
            return false;
        };
        let column = &mut self.columns[ci];
        let ti = column
            .tabs
            .iter()
            .position(|tab| *tab == id)
            .expect("column_of 已确认 id 存在");
        column.tabs.remove(ti);

        if ti < column.top {
            column.top -= 1;
        }
        if column.tabs.is_empty() {
            self.columns.remove(ci);
            if ci < self.focus {
                self.focus -= 1;
            }
            if self.focus >= self.columns.len() && !self.columns.is_empty() {
                self.focus = self.columns.len() - 1;
            }
        } else if column.top >= column.tabs.len() {
            column.top = column.tabs.len() - 1;
        }
        true
    }

    /// 把焦点列左右移动一格；已到边缘时不动。
    pub fn focus_column(&mut self, step: isize) -> bool {
        let next = self.focus as isize + step;
        if next < 0 || next >= self.columns.len() as isize {
            return false;
        }
        self.focus = next as usize;
        true
    }

    /// 在当前列内循环切换栈顶。
    pub fn cycle_tab(&mut self, step: isize) -> Option<u64> {
        self.columns.get_mut(self.focus)?.cycle_top(step)
    }

    /// 把 `id` 并入相邻列，成为目标列的栈顶。
    ///
    /// 这是 scroll 布局下「多个窗口放同一列」的入口。源列只有这一个窗口时，
    /// 整列并过去；目标列变成一个 tab 组。
    pub fn consume(&mut self, id: u64, direction: Direction) -> Option<u64> {
        let source = self.column_of(id)?;
        let target = match direction {
            Direction::Left => source.checked_sub(1)?,
            Direction::Right => (source + 1 < self.columns.len()).then_some(source + 1)?,
            Direction::Up | Direction::Down => return None,
        };

        let column = &mut self.columns[source];
        let index = column.tabs.iter().position(|tab| *tab == id)?;
        column.tabs.remove(index);
        if index < column.top {
            column.top -= 1;
        } else if column.top >= column.tabs.len() {
            column.top = column.tabs.len().saturating_sub(1);
        }

        let removed_column = self.columns[source].tabs.is_empty();
        let target = if removed_column && source < target {
            target - 1
        } else {
            target
        };
        if removed_column {
            self.columns.remove(source);
        }
        let column = &mut self.columns[target];
        column.tabs.push(id);
        column.top = column.tabs.len() - 1;
        self.focus = target;
        Some(id)
    }

    /// 把 `id` 从当前列拆出来，在右侧新开一列。
    pub fn expel(&mut self, id: u64) -> Option<u64> {
        let source = self.column_of(id)?;
        if self.columns[source].tabs.len() <= 1 {
            return None;
        }
        let width = self.columns[source].width;
        let column = &mut self.columns[source];
        let index = column.tabs.iter().position(|tab| *tab == id)?;
        column.tabs.remove(index);
        if index < column.top {
            column.top -= 1;
        } else if column.top >= column.tabs.len() {
            column.top = column.tabs.len().saturating_sub(1);
        }
        self.columns.insert(source + 1, Column::new(width, id));
        self.focus = source + 1;
        Some(id)
    }

    /// 当前焦点列的栈顶窗口。
    pub fn top_window(&self) -> Option<u64> {
        self.columns.get(self.focus)?.top_window()
    }

    /// 各列在行坐标系里的 (x, width)，未加行偏移。
    pub fn column_spans(&self, area_width: i32) -> Vec<(i32, i32)> {
        let mut x = 0;
        self.columns
            .iter()
            .map(|column| {
                let width = column.width.pixels(area_width);
                let span = (x, width);
                x += width;
                span
            })
            .collect()
    }

    /// 把行偏移调整到「焦点列刚好完整可见」的最小位移。
    pub fn scroll_to_focus(&mut self, area: Rect) {
        let Some(&(col_x, col_width)) = self.column_spans(area.width).get(self.focus) else {
            return;
        };
        let left = col_x + self.offset;
        let right = left + col_width;

        if left < 0 {
            self.offset -= left;
        } else if right > area.width {
            self.offset -= right - area.width;
        }
    }

    /// 每个窗口标题条的位置：同一列的 tab 横向平分这条标题带（基线 tabbed）。
    /// 不管是不是栈顶都要有标题，否则切 tab 的入口就没了。
    pub fn title_slices(&self, area: Rect) -> Vec<TitleSlice> {
        let mut slices = Vec::new();
        for ((x, width), column) in self.column_spans(area.width).into_iter().zip(&self.columns) {
            let count = column.tabs.len() as i32;
            if count == 0 {
                continue;
            }
            let left = area.x + x + self.offset;
            for (index, id) in column.tabs.iter().enumerate() {
                let index = index as i32;
                let x0 = left + width * index / count;
                let x1 = left + width * (index + 1) / count;
                slices.push(TitleSlice {
                    id: *id,
                    rect: Rect {
                        x: x0,
                        y: area.y,
                        width: (x1 - x0).max(1),
                        height: area.height,
                    },
                    tab: count > 1,
                    first_tab: index == 0,
                });
            }
        }
        slices
    }

    /// 每一列的栈顶窗口及其目标矩形。
    pub fn target_rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        self.column_spans(area.width)
            .into_iter()
            .zip(&self.columns)
            .filter_map(|((x, width), column)| {
                let window = column.top_window()?;
                Some((
                    window,
                    Rect {
                        x: area.x + x + self.offset,
                        y: area.y,
                        width,
                        height: area.height,
                    },
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
        }
    }

    fn row_of(widths: &[f64]) -> Row {
        let columns = widths
            .iter()
            .enumerate()
            .map(|(i, w)| Column::new(Fraction::new(*w), i as u64))
            .collect();
        Row {
            columns,
            offset: 0,
            focus: 0,
        }
    }

    #[test]
    fn column_width_is_fraction_of_area() {
        let row = row_of(&[0.5, 0.5]);
        assert_eq!(row.column_spans(1000), vec![(0, 500), (500, 500)]);
    }

    #[test]
    fn no_scroll_when_focus_visible() {
        let mut row = row_of(&[0.5, 0.5]);
        row.scroll_to_focus(area());
        assert_eq!(row.offset, 0);
    }

    #[test]
    fn scrolls_to_edge_when_focus_is_right() {
        let mut row = row_of(&[0.5, 0.5, 0.5]);
        row.focus = 2;
        row.scroll_to_focus(area());
        assert_eq!(row.offset, -500);
    }

    #[test]
    fn scrolls_back_to_left_edge() {
        let mut row = row_of(&[0.5, 0.5, 0.5]);
        row.offset = -500;
        row.focus = 0;
        row.scroll_to_focus(area());
        assert_eq!(row.offset, 0);
    }

    #[test]
    fn swapping_exchanges_tab_positions() {
        let mut row = Row::from_ids(&[1, 2], Fraction::new(0.5));
        row.columns[1].tabs.push(3);

        assert!(row.swap(1, 3));
        assert_eq!(row.columns[0].tabs, vec![3]);
        assert_eq!(row.columns[1].tabs, vec![2, 1]);
        assert!(!row.swap(1, 9));
    }

    #[test]
    fn only_top_window_has_rect() {
        let mut row = row_of(&[0.5]);
        row.columns[0].tabs.push(9);
        let rects = row.target_rects(area());
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, 0);
    }

    #[test]
    fn title_slices_split_a_tab_group_evenly() {
        let mut row = row_of(&[0.5, 0.5]);
        row.columns[0].tabs.push(9);
        let slices = row.title_slices(area());
        assert_eq!(slices.len(), 3, "两个 tab + 另一列的一个窗口");
        assert_eq!(slices[0].id, 0);
        assert_eq!(slices[1].id, 9);
        assert_eq!(
            slices[0].rect,
            Rect {
                x: 0,
                y: 0,
                width: 250,
                height: 800
            }
        );
        assert_eq!(
            slices[1].rect,
            Rect {
                x: 250,
                y: 0,
                width: 250,
                height: 800
            }
        );
        assert!(slices[0].tab && slices[0].first_tab);
        assert!(slices[1].tab && !slices[1].first_tab);
        assert!(!slices[2].tab && slices[2].first_tab);
    }

    #[test]
    fn title_slices_follow_the_row_offset() {
        let mut row = row_of(&[0.5, 0.5]);
        row.offset = -300;
        let slices = row.title_slices(area());
        assert_eq!(slices[0].rect.x, -300);
        assert_eq!(slices[1].rect.x, 200);
    }

    #[test]
    fn consume_merges_a_lone_column_into_the_left_one() {
        let mut row = Row::from_ids(&[1, 2], Fraction::new(0.5));
        assert_eq!(row.consume(2, Direction::Left), Some(2));
        assert_eq!(row.columns.len(), 1);
        assert_eq!(row.columns[0].tabs, vec![1, 2]);
        assert_eq!(row.columns[0].top, 1);
        assert_eq!(row.focus, 0);
    }

    #[test]
    fn consume_right_merges_into_the_next_column() {
        let mut row = Row::from_ids(&[1, 2], Fraction::new(0.5));
        assert_eq!(row.consume(1, Direction::Right), Some(1));
        assert_eq!(row.columns.len(), 1);
        assert_eq!(row.columns[0].tabs, vec![2, 1]);
    }

    #[test]
    fn consume_keeps_the_source_column_when_other_tabs_remain() {
        let mut row = Row::from_ids(&[2, 3], Fraction::new(0.5));
        row.columns[0].tabs.insert(0, 1);
        assert_eq!(row.consume(1, Direction::Right), Some(1));
        assert_eq!(row.columns.len(), 2);
        assert_eq!(row.columns[0].tabs, vec![2]);
        assert_eq!(row.columns[1].tabs, vec![3, 1]);
    }

    #[test]
    fn expel_splits_a_tab_into_a_new_column() {
        let mut row = Row::from_ids(&[1], Fraction::new(0.5));
        row.insert_tab(0, 2);
        assert_eq!(row.expel(2), Some(2));
        assert_eq!(row.columns.len(), 2);
        assert_eq!(row.columns[0].tabs, vec![1]);
        assert_eq!(row.columns[1].tabs, vec![2]);
        assert_eq!(row.focus, 1);
    }

    #[test]
    fn resize_changes_column_width_by_pixel_delta() {
        let mut row = Row::from_ids(&[1], Fraction::new(0.5));
        assert!(row.resize(
            1,
            area(),
            crate::layout::ResizeEdges {
                right: true,
                ..Default::default()
            },
            100.0,
        ));
        assert!((row.columns[0].width.value() - 0.6).abs() < 1e-9);
    }
}

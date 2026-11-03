//! BSP 平铺：默认的平铺布局（`docs/adr/0003` 修订）。
//!
//! 递归二分，叶子是窗口。基线的 master/stack 由它取代，并记为欠账
//! （`docs/parity.md`）：观感可以用 split ratio 逼近，但不完全等价。
//!
//! 插入点永远是「当前焦点叶子的位置」：把该叶子换成一个分割节点，原来的窗口
//! 占 `first`、新窗口占 `second`。删除时把父节点折叠成兄弟节点。分割方向按
//! 叶子深度交替（根水平、下一层垂直……），不依赖屏幕尺寸，因此布局层仍然只吃
//! 目标矩形、不碰合成器。

use super::{Direction, Fraction, Rect};

/// 分割方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// 左右分。
    Horizontal,
    /// 上下分。
    Vertical,
}

impl Axis {
    /// 下一层用哪个方向：根水平，之后纵横交替。
    fn next(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

/// 一棵分割树。叶子存窗口 id（由合成器提供的不透明编号）。
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(u64),
    Split {
        axis: Axis,
        ratio: Fraction,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// 分割树 + 新一轮分割的默认比例。空树用 `root: None` 表示。
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub root: Option<Node>,
    pub ratio: Fraction,
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            root: None,
            ratio: Fraction::new(0.5),
        }
    }
}

impl Tree {
    pub fn leaf(id: u64) -> Self {
        Self {
            root: Some(Node::Leaf(id)),
            ratio: Fraction::new(0.5),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// 按给定顺序逐个插入，得到一棵确定的树。
    pub fn from_order(ids: &[u64]) -> Self {
        let mut tree = Self::default();
        let mut focused = None;
        for id in ids {
            tree.insert(focused, *id);
            focused = Some(*id);
        }
        tree
    }

    /// 按深度优先（先 first 后 second）收集叶子，也就是焦点顺序。
    pub fn leaves(&self) -> Vec<u64> {
        fn walk(node: &Node, out: &mut Vec<u64>) {
            match node {
                Node::Leaf(id) => out.push(*id),
                Node::Split { first, second, .. } => {
                    walk(first, out);
                    walk(second, out);
                }
            }
        }

        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, &mut out);
        }
        out
    }

    pub fn contains(&self, id: u64) -> bool {
        self.leaves().contains(&id)
    }

    /// 在焦点叶子的位置插入新窗口；没有焦点（或焦点已不存在）时插到最右叶。
    pub fn insert(&mut self, focused: Option<u64>, id: u64) {
        let target = focused
            .filter(|focused| self.contains(*focused))
            .or_else(|| self.leaves().last().copied());

        match (self.root.take(), target) {
            (None, _) => self.root = Some(Node::Leaf(id)),
            (Some(root), Some(target)) => {
                self.root = Some(replace_leaf(root, target, id, self.ratio));
            }
            (Some(root), None) => {
                // 理论上不会发生：有根就至少有一个叶子。
                self.root = Some(root);
            }
        }
    }

    /// 删掉一个窗口；最后一个窗口删掉后树变成空树。
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        let (new_root, removed) = remove_from(root, id);
        self.root = new_root;
        removed
    }

    /// 交换两个叶子的 id（i3 式拖动平铺窗口）。
    pub fn swap(&mut self, a: u64, b: u64) -> bool {
        if a == b || !self.contains(a) || !self.contains(b) {
            return false;
        }
        fn rename(node: &mut Node, a: u64, b: u64) {
            match node {
                Node::Leaf(id) => {
                    if *id == a {
                        *id = b;
                    } else if *id == b {
                        *id = a;
                    }
                }
                Node::Split { first, second, .. } => {
                    rename(first, a, b);
                    rename(second, a, b);
                }
            }
        }
        if let Some(root) = self.root.as_mut() {
            rename(root, a, b);
        }
        true
    }

    /// 拖动平铺窗口的边时调整离它最近的同轴分割比例。
    ///
    /// `dx`/`dy` 是本次事件的增量（不是相对按下点的总量），由合成器侧的
    /// grab 负责差分；这样多次 motion 不会重复累加。
    pub fn resize(
        &mut self,
        id: u64,
        area: Rect,
        edges: super::ResizeEdges,
        dx: f64,
        dy: f64,
    ) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        resize_node(root, area, id, edges, dx, dy)
    }

    /// 把 `area` 按树结构切成一串目标矩形（顺序与 [`Tree::leaves`] 一致）。
    pub fn target_rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        fn walk(node: &Node, rect: Rect, out: &mut Vec<(u64, Rect)>) {
            match node {
                Node::Leaf(id) => out.push((*id, rect)),
                Node::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => {
                    let (first_rect, second_rect) = split(rect, *axis, *ratio);
                    walk(first, first_rect, out);
                    walk(second, second_rect, out);
                }
            }
        }

        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, area, &mut out);
        }
        out
    }
}

/// 在树里找最近的、与拖动边同轴的分割节点并调整比例。
fn resize_node(
    node: &mut Node,
    rect: Rect,
    target: u64,
    edges: super::ResizeEdges,
    dx: f64,
    dy: f64,
) -> bool {
    let Node::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    else {
        return false;
    };
    let (first_rect, second_rect) = split(rect, *axis, *ratio);
    let first_has = node_contains(first, target);
    let second_has = node_contains(second, target);
    if !first_has && !second_has {
        return false;
    }

    if first_has && resize_node(first, first_rect, target, edges, dx, dy) {
        return true;
    }
    if second_has && resize_node(second, second_rect, target, edges, dx, dy) {
        return true;
    }

    let boundary = match (*axis, first_has) {
        (Axis::Horizontal, true) => edges.right,
        (Axis::Horizontal, false) => edges.left,
        (Axis::Vertical, true) => edges.bottom,
        (Axis::Vertical, false) => edges.top,
    };
    if !boundary {
        return false;
    }
    let (extent, delta) = match *axis {
        Axis::Horizontal => (rect.width as f64, dx),
        Axis::Vertical => (rect.height as f64, dy),
    };
    if extent <= 0.0 {
        return false;
    }
    *ratio = Fraction::new(ratio.value() + delta / extent);
    true
}

fn node_contains(node: &Node, id: u64) -> bool {
    match node {
        Node::Leaf(existing) => *existing == id,
        Node::Split { first, second, .. } => node_contains(first, id) || node_contains(second, id),
    }
}

/// 把叶子换成「原窗口 + 新窗口」的分割节点。
fn replace_leaf(node: Node, target: u64, id: u64, ratio: Fraction) -> Node {
    match node {
        Node::Leaf(existing) if existing == target => Node::Split {
            axis: Axis::Horizontal,
            ratio,
            first: Box::new(Node::Leaf(existing)),
            second: Box::new(Node::Leaf(id)),
        },
        Node::Leaf(existing) => Node::Leaf(existing),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let depth_axis = axis.next();
            Node::Split {
                axis,
                ratio,
                first: Box::new(replace_at_depth(*first, target, id, depth_axis, ratio)),
                second: Box::new(replace_at_depth(*second, target, id, depth_axis, ratio)),
            }
        }
    }
}

/// 与 [`replace_leaf`] 相同，但已知这一层该用哪个方向。
fn replace_at_depth(node: Node, target: u64, id: u64, axis: Axis, ratio: Fraction) -> Node {
    match node {
        Node::Leaf(existing) if existing == target => Node::Split {
            axis,
            ratio,
            first: Box::new(Node::Leaf(existing)),
            second: Box::new(Node::Leaf(id)),
        },
        Node::Leaf(existing) => Node::Leaf(existing),
        Node::Split {
            axis: own_axis,
            ratio: own_ratio,
            first,
            second,
        } => {
            let next_axis = own_axis.next();
            Node::Split {
                axis: own_axis,
                ratio: own_ratio,
                first: Box::new(replace_at_depth(*first, target, id, next_axis, ratio)),
                second: Box::new(replace_at_depth(*second, target, id, next_axis, ratio)),
            }
        }
    }
}

fn remove_from(node: Node, id: u64) -> (Option<Node>, bool) {
    match node {
        Node::Leaf(existing) if existing == id => (None, true),
        Node::Leaf(existing) => (Some(Node::Leaf(existing)), false),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (new_first, removed) = remove_from(*first, id);
            if removed {
                return match new_first {
                    Some(first) => (
                        Some(Node::Split {
                            axis,
                            ratio,
                            first: Box::new(first),
                            second,
                        }),
                        true,
                    ),
                    None => (Some(*second), true),
                };
            }

            let (new_second, removed) = remove_from(*second, id);
            match (new_second, removed) {
                (Some(second), true) => (
                    Some(Node::Split {
                        axis,
                        ratio,
                        first: Box::new(new_first.expect("first 未删除时必然还在")),
                        second: Box::new(second),
                    }),
                    true,
                ),
                (None, true) => (new_first, true),
                (_, false) => unreachable!("remove_from 只在找到 id 时返回 true"),
            }
        }
    }
}

/// 按比例切分矩形，两个子矩形严格拼回原矩形。
fn split(rect: Rect, axis: Axis, ratio: Fraction) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let width = clamp_cut(ratio.pixels(rect.width), rect.width);
            (
                Rect { width, ..rect },
                Rect {
                    x: rect.x + width,
                    width: rect.width - width,
                    ..rect
                },
            )
        }
        Axis::Vertical => {
            let height = clamp_cut(ratio.pixels(rect.height), rect.height);
            (
                Rect { height, ..rect },
                Rect {
                    y: rect.y + height,
                    height: rect.height - height,
                    ..rect
                },
            )
        }
    }
}

/// 至少留 1 px 给每个子矩形，避免比例为 0 时出现零宽窗口。
fn clamp_cut(cut: i32, extent: i32) -> i32 {
    if extent >= 2 {
        cut.clamp(1, extent - 1)
    } else {
        cut.clamp(0, extent.max(0))
    }
}

/// 在目标矩形集合里找 `current` 在指定方向上的邻居。
///
/// 优先选「另一轴有重叠」的矩形（真正的相邻列/行），距离相同再看重叠长度；
/// 没有正对邻居时退化为「中心落在该方向、欧氏距离最近」。
pub fn neighbor(rects: &[(u64, Rect)], current: u64, direction: Direction) -> Option<u64> {
    let (_, cur) = rects.iter().find(|(id, _)| *id == current)?;

    let mut best: Option<(u64, i32, i32)> = None;
    for (id, rect) in rects {
        if *id == current {
            continue;
        }

        let overlap = match direction {
            Direction::Left | Direction::Right => overlap_y(cur, rect),
            Direction::Up | Direction::Down => overlap_x(cur, rect),
        };
        let (aligned, distance) = match direction {
            Direction::Left => (rect.x + rect.width <= cur.x, cur.x - (rect.x + rect.width)),
            Direction::Right => (rect.x >= cur.x + cur.width, rect.x - (cur.x + cur.width)),
            Direction::Up => (
                rect.y + rect.height <= cur.y,
                cur.y - (rect.y + rect.height),
            ),
            Direction::Down => (rect.y >= cur.y + cur.height, rect.y - (cur.y + cur.height)),
        };

        if !aligned || overlap <= 0 {
            continue;
        }

        let score = (distance, -overlap);
        if best.is_none_or(|(_, best_distance, best_overlap)| score < (best_distance, best_overlap))
        {
            best = Some((*id, score.0, score.1));
        }
    }

    if let Some((id, _, _)) = best {
        return Some(id);
    }

    // 退化路径：没有正对邻居时按中心点方向找最近的。
    let cur_center = center(cur);
    rects
        .iter()
        .filter(|(id, _)| *id != current)
        .filter_map(|(id, rect)| {
            let other = center(rect);
            let (dx, dy) = (other.0 - cur_center.0, other.1 - cur_center.1);
            let in_direction = match direction {
                Direction::Left => dx < 0,
                Direction::Right => dx > 0,
                Direction::Up => dy < 0,
                Direction::Down => dy > 0,
            };
            in_direction.then_some((*id, dx * dx + dy * dy))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(id, _)| id)
}

fn center(rect: &Rect) -> (i32, i32) {
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

fn overlap_x(a: &Rect, b: &Rect) -> i32 {
    overlap(a.x, a.width, b.x, b.width)
}

fn overlap_y(a: &Rect, b: &Rect) -> i32 {
    overlap(a.y, a.height, b.y, b.height)
}

fn overlap(a_pos: i32, a_len: i32, b_pos: i32, b_len: i32) -> i32 {
    (a_pos + a_len).min(b_pos + b_len) - a_pos.max(b_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }
    }

    #[test]
    fn empty_tree_has_no_leaves_or_rects() {
        let tree = Tree::default();
        assert!(tree.is_empty());
        assert!(tree.leaves().is_empty());
        assert!(tree.target_rects(area()).is_empty());
    }

    #[test]
    fn leaves_are_depth_first() {
        let tree = Tree {
            root: Some(Node::Split {
                axis: Axis::Horizontal,
                ratio: Fraction::new(0.5),
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Split {
                    axis: Axis::Vertical,
                    ratio: Fraction::new(0.5),
                    first: Box::new(Node::Leaf(2)),
                    second: Box::new(Node::Leaf(3)),
                }),
            }),
            ratio: Fraction::new(0.5),
        };
        assert_eq!(tree.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn target_rects_split_along_both_axes() {
        let tree = Tree {
            root: Some(Node::Split {
                axis: Axis::Horizontal,
                ratio: Fraction::new(0.5),
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Split {
                    axis: Axis::Vertical,
                    ratio: Fraction::new(0.5),
                    first: Box::new(Node::Leaf(2)),
                    second: Box::new(Node::Leaf(3)),
                }),
            }),
            ratio: Fraction::new(0.5),
        };

        let rects = tree.target_rects(area());
        assert_eq!(
            rects,
            vec![
                (
                    1,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 50,
                        height: 100
                    }
                ),
                (
                    2,
                    Rect {
                        x: 50,
                        y: 0,
                        width: 50,
                        height: 50
                    }
                ),
                (
                    3,
                    Rect {
                        x: 50,
                        y: 50,
                        width: 50,
                        height: 50
                    }
                ),
            ]
        );
    }

    #[test]
    fn insert_splits_the_focused_leaf() {
        let mut tree = Tree::default();
        tree.insert(None, 1);
        tree.insert(Some(1), 2);
        tree.insert(Some(2), 3);
        assert_eq!(tree.leaves(), vec![1, 2, 3]);
        assert_eq!(tree.target_rects(area()).len(), 3);
    }

    #[test]
    fn insert_without_focus_appends_to_last_leaf() {
        let mut tree = Tree::from_order(&[1, 2]);
        tree.insert(None, 3);
        assert_eq!(tree.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn removing_a_leaf_collapses_its_parent() {
        let mut tree = Tree::from_order(&[1, 2, 3]);
        assert!(tree.remove(2));
        assert_eq!(tree.leaves(), vec![1, 3]);
        assert_eq!(tree.target_rects(area()).len(), 2);
    }

    #[test]
    fn removing_everything_empties_the_tree() {
        let mut tree = Tree::from_order(&[1, 2]);
        assert!(tree.remove(1));
        assert!(tree.remove(2));
        assert!(tree.is_empty());
        assert!(!tree.remove(2));
    }

    #[test]
    fn neighbor_follows_the_split() {
        let tree = Tree {
            root: Some(Node::Split {
                axis: Axis::Horizontal,
                ratio: Fraction::new(0.5),
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Split {
                    axis: Axis::Vertical,
                    ratio: Fraction::new(0.5),
                    first: Box::new(Node::Leaf(2)),
                    second: Box::new(Node::Leaf(3)),
                }),
            }),
            ratio: Fraction::new(0.5),
        };
        let rects = tree.target_rects(area());

        assert_eq!(neighbor(&rects, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&rects, 2, Direction::Left), Some(1));
        assert_eq!(neighbor(&rects, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&rects, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&rects, 1, Direction::Left), None);
    }

    #[test]
    fn swapping_leaves_exchanges_their_slots() {
        let mut tree = Tree::from_order(&[1, 2, 3]);
        assert!(tree.swap(1, 3));
        assert_eq!(tree.leaves(), vec![3, 2, 1]);
        assert!(!tree.swap(1, 9));
    }

    #[test]
    fn split_never_produces_negative_rects() {
        let tree = Tree {
            root: Some(Node::Split {
                axis: Axis::Horizontal,
                ratio: Fraction::new(Fraction::MIN),
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }),
            ratio: Fraction::new(0.5),
        };
        let rects = tree.target_rects(Rect {
            width: 1,
            height: 1,
            ..area()
        });
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].1.width, 0);
        assert_eq!(rects[1].1.width, 1);
        assert!(rects.iter().all(|(_, rect)| rect.width >= 0));
    }
    #[test]
    fn resize_changes_the_nearest_split_ratio() {
        let mut tree = Tree::from_order(&[1, 2]);
        assert!(tree.resize(
            1,
            area(),
            crate::layout::ResizeEdges {
                right: true,
                ..Default::default()
            },
            20.0,
            0.0,
        ));
        let rects = tree.target_rects(area());
        assert_eq!(rects[0].1.width, 70);
        assert_eq!(rects[1].1.width, 30);
    }
}

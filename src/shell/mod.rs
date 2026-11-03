//! 窗口外壳：窗口表、层、tag 与焦点栈。
//!
//! 层序（`CONTEXT.md`「平铺层 / 浮动层」）：全屏 > 浮动层 > 平铺层。
//! 窗口的屏幕坐标由 `Space` 保管（它是渲染与命中测试的唯一真相），本模块只记
//! 与布局有关的元数据：所在层、所属 tag、焦点顺序。
//!
//! 布局结构（每个 tag 一棵 bsp 树 / 一行 scroll 列）在 [`WorkspaceSet`] 里，
//! 这样 tag 与几何逻辑可以脱离 `Window` 单测。本模块负责把窗口 id 与
//! 布局、焦点栈缝起来。

use smithay::{
    desktop::Window, reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::seat::WaylandFocus,
};

use std::collections::HashMap;

use crate::layout::{Direction, Fraction, Rect, TiledLayout, TitleSlice, WorkspaceSet, lowest_tag};

/// 窗口编号：合成器内部的不透明 id，进入布局层后就是 `u64`。
pub type WindowId = u64;

/// 窗口所在层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    Tiled,
    Floating,
    Fullscreen,
}

/// 窗口表里的一条记录。
pub struct ManagedWindow {
    pub id: WindowId,
    pub window: Window,
    /// 窗口归属的输出名；多输出下每个窗口只属于一个输出。
    pub output: String,
    pub layer: Layer,
    /// 归属 tag 的位掩码；同一时刻只在「归属行」里参与布局（`CONTEXT.md`）。
    pub tags: u32,
    /// 浮动窗口的 sticky：永远可见（不属于任何 tag）且永远置顶。
    pub sticky: bool,
    /// 是否已经按自身尺寸摆放过（首帧提交后才知道尺寸）。
    pub placed: bool,
    /// 浮动层的左上角；重新映射时用它恢复位置。
    pub floating_loc: Option<(i32, i32)>,
    /// 最近一次为平铺窗口发送的 configure 尺寸，避免重复 configure。
    pub configured_size: Option<(i32, i32)>,
    /// 进入全屏前的层与浮动位置；退出全屏时恢复。
    pub fullscreen_restore: Option<(Layer, Option<(i32, i32)>)>,
    /// 客户端最近上报的标题；IPC 事件用它判断是否真的变了。
    pub title: Option<String>,
    /// 客户端最近上报的 app_id；只用于查询。
    pub app_id: Option<String>,
}

/// 焦点栈：最近使用在前。
///
/// 单独抽出来是为了能脱离 `Window` 单测——窗口句柄在测试里构造不出来。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FocusStack {
    order: Vec<WindowId>,
}

impl FocusStack {
    pub fn new(order: Vec<WindowId>) -> Self {
        Self { order }
    }

    pub fn current(&self) -> Option<WindowId> {
        self.order.first().copied()
    }

    /// 最近使用在前。
    pub fn ids(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.order.iter().copied()
    }

    /// 把某窗口提到栈顶。
    pub fn set(&mut self, id: WindowId) {
        self.order.retain(|other| *other != id);
        self.order.insert(0, id);
    }

    pub fn remove(&mut self, id: WindowId) {
        self.order.retain(|other| *other != id);
    }

    pub fn clear(&mut self) {
        self.order.clear();
    }

    /// 丢掉不再存活的 id。
    pub fn prune(&mut self, alive: impl Fn(WindowId) -> bool) {
        self.order.retain(|id| alive(*id));
    }

    /// 按栈顺序循环，`step` 为正表示更早使用。
    pub fn cycle(&self, step: isize) -> Option<WindowId> {
        if self.order.is_empty() {
            return None;
        }
        let len = self.order.len() as isize;
        Some(self.order[step.rem_euclid(len) as usize])
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// 窗口表 + 焦点栈 + 每个 tag 的布局。
pub struct Shell {
    next_id: WindowId,
    windows: Vec<ManagedWindow>,
    focus: FocusStack,
    /// 每个输出一套 tag/布局状态（`CONTEXT.md`「行按输出分组」）。
    layouts: HashMap<String, WorkspaceSet>,
    default_layout: TiledLayout,
    default_width: Fraction,
}

impl Shell {
    pub fn new(default_layout: TiledLayout, default_width: Fraction) -> Self {
        Self {
            next_id: 1,
            windows: Vec::new(),
            focus: FocusStack::default(),
            layouts: HashMap::new(),
            default_layout,
            default_width,
        }
    }

    /// 输出上线：为它建立独立的可见 tag 与布局状态。
    pub fn add_output(&mut self, output: &str) {
        self.layouts
            .entry(output.to_string())
            .or_insert_with(|| WorkspaceSet::new(self.default_layout, self.default_width));
    }

    /// 输出下线：把布局状态移除；窗口本身由调用方决定重新归属还是关掉。
    pub fn remove_output(&mut self, output: &str) {
        self.layouts.remove(output);
    }

    /// 某个输出是否有可见窗口。
    pub fn has_output(&self, output: &str) -> bool {
        self.layouts.contains_key(output)
    }

    /// 把窗口迁到另一个输出（输出拔掉时的兜底）。
    pub fn move_to_output(&mut self, id: WindowId, new_output: &str) {
        let Some(entry) = self.find(id) else {
            return;
        };
        if entry.output == new_output {
            return;
        }
        let old_output = entry.output.clone();
        let layer = entry.layer;
        let tags = entry.tags;
        let home = self
            .layouts(&old_output)
            .and_then(|layouts| layouts.home_of(id));

        if layer == Layer::Tiled
            && let Some(home) = home
            && let Some(layouts) = self.layouts_mut(&old_output)
        {
            layouts.remove(home, id);
        }
        if let Some(entry) = self.find_mut(id) {
            entry.output = new_output.to_string();
            entry.configured_size = None;
        }
        if layer == Layer::Tiled {
            let target = self
                .layouts(new_output)
                .map(|layouts| layouts.focused_tag())
                .filter(|tag| tags & tag != 0)
                .unwrap_or(tags);
            let focused = self
                .mru_in_tag(new_output, target)
                .filter(|focused| *focused != id);
            if let Some(layouts) = self.layouts_mut(new_output) {
                layouts.insert(target, focused, id);
            }
        }
    }

    fn layouts(&self, output: &str) -> Option<&WorkspaceSet> {
        self.layouts.get(output)
    }

    fn layouts_mut(&mut self, output: &str) -> Option<&mut WorkspaceSet> {
        self.layouts.get_mut(output)
    }

    /// 登记一个新窗口，并让它获得焦点。
    pub fn add(&mut self, window: Window, layer: Layer, tags: u32, output: &str) -> WindowId {
        let id = self.next_id;
        self.next_id += 1;

        let focused = (layer == Layer::Tiled)
            .then(|| self.mru_in_tag(output, tags))
            .flatten();
        if layer == Layer::Tiled {
            // 输出可能刚上线、还没经过 `add_output`；按需补一套布局状态。
            self.layouts
                .entry(output.to_string())
                .or_insert_with(|| WorkspaceSet::new(self.default_layout, self.default_width))
                .insert(tags, focused, id);
        }

        self.windows.push(ManagedWindow {
            id,
            window,
            output: output.to_string(),
            layer,
            tags,
            sticky: false,
            placed: false,
            floating_loc: None,
            configured_size: None,
            fullscreen_restore: None,
            title: None,
            app_id: None,
        });
        self.focus.set(id);
        id
    }

    /// 按窗口句柄移除，返回被移除的 id。
    pub fn remove(&mut self, window: &Window) -> Option<WindowId> {
        let id = self.id_of_window(window)?;
        self.remove_by_id(id)
    }

    pub fn remove_by_id(&mut self, id: WindowId) -> Option<WindowId> {
        let index = self.windows.iter().position(|w| w.id == id)?;
        let removed = self.windows.remove(index);
        self.focus.remove(id);
        if removed.layer == Layer::Tiled
            && let Some(layouts) = self.layouts_mut(&removed.output)
        {
            layouts.remove(removed.tags, id);
        }
        Some(id)
    }

    pub fn find(&self, id: WindowId) -> Option<&ManagedWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn find_mut(&mut self, id: WindowId) -> Option<&mut ManagedWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn id_of_window(&self, window: &Window) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|w| &w.window == window)
            .map(|w| w.id)
    }

    /// 按顶层 surface 找窗口。X11 窗口用 XWayland 自己的 wl_surface 关联。
    pub fn id_of_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|w| w.window.wl_surface().as_deref() == Some(surface))
            .map(|w| w.id)
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.find(id).map(|w| &w.window)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ManagedWindow> {
        self.windows.iter()
    }

    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().map(|w| &w.window)
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// 焦点栈顺序（最近使用在前）；切换窗口的候选按它排列。
    pub fn focus_order(&self) -> Vec<WindowId> {
        self.focus.ids().collect()
    }

    /// 当前焦点窗口；已经被移除的 id 会从栈里清掉。
    pub fn focus(&mut self) -> Option<WindowId> {
        self.focus
            .prune(|id| self.windows.iter().any(|w| w.id == id));
        self.focus.current()
    }

    pub fn set_focus(&mut self, id: WindowId) {
        self.focus.set(id);
    }

    pub fn clear_focus(&mut self) {
        self.focus.clear();
    }

    /// 焦点按栈顺序在**可见**窗口之间循环，`step` 为正表示向后（更早使用）。
    pub fn cycle_focus(&mut self, output: &str, step: isize) -> Option<WindowId> {
        self.focus();
        let visible = self.visible_tags(output);
        let candidates: Vec<WindowId> = self
            .focus
            .ids()
            .filter(|id| {
                self.find(*id).is_some_and(|entry| {
                    entry.output == output && tag_visible(entry.tags, entry.sticky, visible)
                })
            })
            .collect();

        let current = self.focus.current()?;
        let index = candidates.iter().position(|id| *id == current).unwrap_or(0);
        let len = candidates.len() as isize;
        if len == 0 {
            return None;
        }
        Some(candidates[(index as isize + step).rem_euclid(len) as usize])
    }

    // ---- tag 与布局 ----

    pub fn visible_tags(&self, output: &str) -> u32 {
        self.layouts(output).map(|l| l.visible_tags()).unwrap_or(1)
    }

    pub fn set_visible_tags(&mut self, output: &str, mask: u32) {
        if let Some(layouts) = self.layouts_mut(output) {
            layouts.set_visible_tags(mask);
        }
        self.rehome_visible(output);
    }

    pub fn toggle_visible_tags(&mut self, output: &str, mask: u32) {
        if let Some(layouts) = self.layouts_mut(output) {
            layouts.toggle_visible_tags(mask);
        }
        self.rehome_visible(output);
    }

    /// 在当前/上一套可见集合之间切换（`view_prev`）。
    pub fn swap_visible_tags(&mut self, output: &str) -> bool {
        let swapped = self
            .layouts_mut(output)
            .is_some_and(|layouts| layouts.swap_visible_tags());
        if swapped {
            self.rehome_visible(output);
        }
        swapped
    }

    /// 可见集合整体平移一步（基线 `shiftview`）：取集合里最高/最低的 tag 加上
    /// `step`，越界就不动。bar 的 tag 区上滚轮走这里。
    pub fn shift_view(&mut self, output: &str, step: i32) -> bool {
        // 与基线的 9 个 tag 一致。
        const TAGS: i32 = 9;
        let set = self.visible_tags(output);
        let current = if step > 0 {
            (0..TAGS).rev().find(|bit| set & (1 << bit) != 0)
        } else {
            (0..TAGS).find(|bit| set & (1 << bit) != 0)
        };
        let Some(current) = current else {
            return false;
        };
        let next = current + step;
        if next == current || !(0..TAGS).contains(&next) {
            return false;
        }
        self.set_visible_tags(output, 1 << next);
        true
    }

    pub fn focused_tag(&self, output: &str) -> u32 {
        self.layouts(output).map(|l| l.focused_tag()).unwrap_or(1)
    }

    /// 当前 tag 的布局（`setlayout`）。
    pub fn set_layout(&mut self, output: &str, layout: TiledLayout) {
        if let Some(layouts) = self.layouts_mut(output) {
            let tag = layouts.focused_tag();
            layouts.set_layout(tag, layout);
        }
    }

    /// 某个 tag 的布局；没有窗口时报告默认布局。
    pub fn layout_of(&self, output: &str, tag: u32) -> TiledLayout {
        self.layouts(output)
            .map(|l| l.layout_of(tag))
            .unwrap_or(self.default_layout)
    }

    /// 只影响之后新建/tag 首次使用的布局。
    pub fn set_default_layout(&mut self, layout: TiledLayout) {
        self.default_layout = layout;
        for layouts in self.layouts.values_mut() {
            layouts.set_default_layout(layout);
        }
    }

    /// 重新计算滚动偏移；必须在 [`Shell::arrange_targets`] 之前调用。
    pub fn relayout(&mut self, output: &str, work: Rect) {
        if let Some(layouts) = self.layouts_mut(output) {
            layouts.relayout(work);
        }
    }

    /// 本轮 arrange 的平铺目标矩形。
    pub fn arrange_targets(&self, output: &str, work: Rect) -> Vec<(WindowId, Rect)> {
        self.layouts(output)
            .map(|l| l.arrange_targets(work))
            .unwrap_or_default()
    }

    /// 本轮 arrange 的标题条位置；选中态由调用方按焦点栈补上。
    pub fn title_slices(&self, output: &str, work: Rect) -> Vec<TitleSlice> {
        self.layouts(output)
            .map(|l| l.title_slices(work))
            .unwrap_or_default()
    }

    /// 焦点落到当前窗口后，同步 tag 行、列焦点与滚动偏移。
    pub fn note_focus(&mut self, output: &str, work: Rect) {
        let Some(id) = self.focus() else {
            return;
        };
        let Some(entry) = self.find(id) else {
            return;
        };
        if entry.layer == Layer::Tiled
            && entry.output == output
            && let Some(layouts) = self.layouts_mut(output)
        {
            layouts.note_focus(id, work);
        }
    }

    /// 可见行之间移动焦点（`focus_row_*`）。单行时不动，到边缘不循环。
    pub fn focus_row(&mut self, output: &str, step: isize) -> Option<WindowId> {
        if self.focus_frozen() {
            return None;
        }
        let layouts = self.layouts(output)?;
        let tags = layouts.visible_tag_list();
        if tags.len() < 2 {
            return None;
        }

        let current = tags
            .iter()
            .position(|tag| *tag == layouts.focused_tag())
            .unwrap_or(0) as isize;
        let next = (current + step).clamp(0, tags.len() as isize - 1) as usize;
        if next == current as usize {
            return None;
        }

        let tag = tags[next];
        let id = self.mru_in_tag(output, tag).or_else(|| {
            layouts
                .workspace(tag)
                .and_then(|workspace| workspace.top_window())
        })?;
        if let Some(layouts) = self.layouts_mut(output) {
            layouts.set_focused_tag(tag);
        }
        self.focus.set(id);
        Some(id)
    }

    /// 当前行内的左右导航。焦点在浮动窗口上时冻结（`CONTEXT.md`「冻结」）。
    pub fn focus_column(
        &mut self,
        output: &str,
        direction: Direction,
        work: Rect,
    ) -> Option<WindowId> {
        if self.focus_frozen() {
            return None;
        }
        let focused = self.focus();
        let id = self
            .layouts_mut(output)?
            .focus_column(direction, focused, work)?;
        self.focus.set(id);
        Some(id)
    }

    /// 当前列的 tab 循环（bsp 下退化成焦点顺序循环）。浮动焦点上冻结。
    pub fn cycle_tab(&mut self, output: &str, step: isize) -> Option<WindowId> {
        if self.focus_frozen() {
            return None;
        }
        let focused = self.focus();
        let id = self.layouts_mut(output)?.cycle_tab(focused, step)?;
        self.focus.set(id);
        Some(id)
    }

    /// 把焦点窗口的 tag 归属整体替换成掩码（`tag` / `transfer_to`）。
    pub fn transfer(&mut self, id: WindowId, mask: u32) -> Option<u32> {
        if mask == 0 {
            return None;
        }
        self.set_tags(id, mask);
        Some(mask)
    }

    /// 切换焦点窗口的 tag 归属（异或，基线 `toggletag`）；结果为空时忽略。
    pub fn toggle_tags(&mut self, id: WindowId, mask: u32) -> bool {
        let Some(tags) = self.find(id).map(|entry| entry.tags ^ mask) else {
            return false;
        };
        if tags == 0 {
            return false;
        }
        self.set_tags(id, tags)
    }

    /// 改写窗口的 tag 掩码，并把它挂到合适的可见行。
    ///
    /// 多 tag 归属在本项目里的语义（`CONTEXT.md`「归属行」）：窗口同一时刻只
    /// 出现在一个行里——当前行仍匹配就留在原行，否则取掩码里最低的可见 tag；
    /// 一个都不可见时保留原行，等切回来再显示。
    pub fn set_tags(&mut self, id: WindowId, mask: u32) -> bool {
        if mask == 0 {
            return false;
        }
        let Some(entry) = self.find(id) else {
            return false;
        };
        if entry.tags == mask {
            return false;
        }
        let layer = entry.layer;
        let output = entry.output.clone();

        let old_home = self
            .layouts(&output)
            .and_then(|layouts| layouts.home_of(id));
        let visible = self.visible_tags(&output);
        let new_home = old_home
            .filter(|home| home & mask != 0 && home & visible != 0)
            .or_else(|| lowest_tag(mask & visible))
            .or_else(|| lowest_tag(mask));

        if layer == Layer::Tiled && new_home != old_home {
            let focused = new_home
                .and_then(|tag| self.mru_in_tag(&output, tag))
                .filter(|focused| *focused != id);
            if let Some(layouts) = self.layouts_mut(&output) {
                if let Some(old) = old_home {
                    layouts.remove(old, id);
                }
                if let Some(new) = new_home {
                    layouts.insert(new, focused, id);
                }
            }
        }
        if let Some(entry) = self.find_mut(id) {
            entry.tags = mask;
        }
        true
    }

    /// 可见集合变化后，把「家」已不可见但仍有可见 tag 的平铺窗口换到新行。
    fn rehome_visible(&mut self, output: &str) {
        let visible = self.visible_tags(output);
        let moves: Vec<(WindowId, u32, u32)> = self
            .windows
            .iter()
            .filter(|entry| entry.layer == Layer::Tiled && entry.output == output)
            .filter_map(|entry| {
                let home = self.layouts(output)?.home_of(entry.id)?;
                if home & visible != 0 {
                    return None;
                }
                let target = lowest_tag(entry.tags & visible)?;
                (target != home).then_some((entry.id, home, target))
            })
            .collect();

        for (id, from, to) in moves {
            let focused = self.mru_in_tag(output, to).filter(|focused| *focused != id);
            if let Some(layouts) = self.layouts_mut(output) {
                layouts.remove(from, id);
                layouts.insert(to, focused, id);
            }
        }
    }

    /// 平铺层与浮动层互转。返回是否真的换了层。
    ///
    /// 用窗口的「归属行」而不是最低 tag：多 tag 窗口可能正挂在第 2 个 bit 上。
    /// 离开浮动层时 sticky 一并取消。
    pub fn set_layer(&mut self, id: WindowId, layer: Layer) -> bool {
        let Some(entry) = self.find(id) else {
            return false;
        };
        if entry.layer == layer {
            return false;
        }
        let tags = entry.tags;
        let output = entry.output.clone();
        let home = self
            .layouts(&output)
            .and_then(|layouts| layouts.home_of(id));
        let was_tiled = entry.layer == Layer::Tiled;
        let visible = self.visible_tags(&output);

        let target_home = home
            .filter(|tag| tag & tags != 0)
            .or_else(|| lowest_tag(tags & visible))
            .or_else(|| lowest_tag(tags));
        let focused = target_home.and_then(|tag| {
            self.mru_in_tag(&output, tag)
                .filter(|focused| *focused != id)
        });

        if was_tiled
            && let Some(tag) = home
            && let Some(layouts) = self.layouts_mut(&output)
        {
            layouts.remove(tag, id);
        }
        if let Some(entry) = self.find_mut(id) {
            entry.layer = layer;
            entry.placed = true;
            entry.sticky = entry.sticky && layer == Layer::Floating;
        }
        if layer == Layer::Tiled
            && let Some(tag) = target_home
            && let Some(layouts) = self.layouts_mut(&output)
        {
            layouts.insert(tag, focused, id);
        }
        true
    }

    /// 交互缩放平铺窗口：只改布局比例/列宽，不把窗口转成浮动。
    pub fn resize_tiled(
        &mut self,
        id: WindowId,
        work: Rect,
        edges: crate::layout::ResizeEdges,
        dx: f64,
        dy: f64,
    ) -> bool {
        let Some(entry) = self.find(id) else {
            return false;
        };
        if entry.layer != Layer::Tiled {
            return false;
        }
        let output = entry.output.clone();
        self.layouts_mut(&output)
            .is_some_and(|layouts| layouts.resize(id, work, edges, dx, dy))
    }

    /// scroll 布局下把窗口并入相邻列。
    pub fn consume_tiled(&mut self, id: WindowId, direction: Direction) -> bool {
        let Some(entry) = self.find(id) else {
            return false;
        };
        if entry.layer != Layer::Tiled {
            return false;
        }
        let output = entry.output.clone();
        self.layouts_mut(&output)
            .is_some_and(|layouts| layouts.consume(id, direction))
    }

    /// scroll 布局下把窗口从当前列拆到右侧新列。
    pub fn expel_tiled(&mut self, id: WindowId) -> bool {
        let Some(entry) = self.find(id) else {
            return false;
        };
        if entry.layer != Layer::Tiled {
            return false;
        }
        let output = entry.output.clone();
        self.layouts_mut(&output)
            .is_some_and(|layouts| layouts.expel(id))
    }

    /// 交换两个平铺窗口的布局位置（拖动平铺窗口时使用）。
    pub fn swap_tiles(&mut self, a: WindowId, b: WindowId) -> bool {
        if a == b {
            return false;
        }
        let tiled = |entry: &ManagedWindow| entry.layer == Layer::Tiled;
        if !self.find(a).is_some_and(tiled) || !self.find(b).is_some_and(tiled) {
            return false;
        }
        let Some(output) = self.find(a).map(|entry| entry.output.clone()) else {
            return false;
        };
        self.layouts_mut(&output).is_some_and(|l| l.swap(a, b))
    }

    /// 浮动窗口的 sticky 开关：置顶且在所有 tag 可见。
    ///
    /// 只有浮动层窗口可以置顶；平铺/全屏窗口返回 `None`，由调用方决定怎么提示。
    /// 返回切换后的状态。
    pub fn toggle_sticky(&mut self, id: WindowId) -> Option<bool> {
        let entry = self.find(id)?;
        if entry.layer != Layer::Floating {
            return None;
        }
        let sticky = !entry.sticky;
        if let Some(entry) = self.find_mut(id) {
            entry.sticky = sticky;
        }
        Some(sticky)
    }

    /// 焦点兜底：当前焦点不可见时，依次尝试当前行栈顶、任意可见窗口。
    pub fn fallback_focus(&self, output: &str, visible: u32) -> Option<WindowId> {
        self.layouts(output)
            .and_then(|layouts| {
                layouts
                    .workspace(layouts.focused_tag())
                    .and_then(|workspace| workspace.top_window())
            })
            .filter(|id| {
                self.find(*id).is_some_and(|entry| {
                    entry.output == output && tag_visible(entry.tags, entry.sticky, visible)
                })
            })
            .or_else(|| {
                self.focus.ids().find(|id| {
                    self.find(*id).is_some_and(|entry| {
                        entry.output == output && tag_visible(entry.tags, entry.sticky, visible)
                    })
                })
            })
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|entry| {
                        entry.output == output && tag_visible(entry.tags, entry.sticky, visible)
                    })
                    .map(|entry| entry.id)
            })
    }

    /// 某个 tag 里最近使用的平铺窗口。
    ///
    /// 还要核对「归属行」：多 tag 窗口只在自己的行里被当作候选，否则
    /// `focus_row` 会把它从别的行抢过来。
    fn mru_in_tag(&self, output: &str, tag: u32) -> Option<WindowId> {
        self.focus.ids().find(|id| {
            self.find(*id).is_some_and(|entry| {
                entry.output == output
                    && entry.tags & tag != 0
                    && entry.layer == Layer::Tiled
                    && self
                        .layouts(output)
                        .and_then(|layouts| layouts.home_of(*id))
                        .is_none_or(|home| home == tag)
            })
        })
    }

    /// 焦点是否停在浮动/全屏窗口上——此时方向键不动作。
    fn focus_frozen(&mut self) -> bool {
        self.focus()
            .and_then(|id| self.find(id))
            .is_some_and(|entry| entry.layer != Layer::Tiled)
    }
}

/// 窗口在当前可见集合里是否应该被映射；sticky 浮动窗口永远可见。
///
/// 单独抽成函数是为了能脱离 `Window` 单测（窗口句柄在测试里构造不出来）。
pub fn tag_visible(tags: u32, sticky: bool, visible: u32) -> bool {
    sticky || tags & visible != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_prefers_most_recent() {
        let mut stack = FocusStack::new(vec![3, 2, 1]);
        assert_eq!(stack.current(), Some(3));
        stack.set(2);
        assert_eq!(stack.current(), Some(2));
        assert_eq!(stack.len(), 3);
    }

    #[test]
    fn focus_prunes_dead_ids() {
        let mut stack = FocusStack::new(vec![9, 1]);
        stack.prune(|id| id != 9);
        assert_eq!(stack.current(), Some(1));
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn cycle_wraps_both_directions() {
        let stack = FocusStack::new(vec![1, 2, 3]);
        assert_eq!(stack.cycle(1), Some(2));
        assert_eq!(stack.cycle(-1), Some(3));
    }

    #[test]
    fn cycle_on_empty_stack_is_none() {
        let stack = FocusStack::new(Vec::new());
        assert_eq!(stack.cycle(1), None);
    }

    #[test]
    fn sticky_is_visible_on_every_tag() {
        // 窗口归属 tag 1，但切到 tag 2 时 sticky 仍然显示。
        assert!(tag_visible(0b0001, false, 0b0001));
        assert!(!tag_visible(0b0001, false, 0b0010));
        assert!(tag_visible(0b0001, true, 0b0010));
    }

    #[test]
    fn shift_view_moves_the_visible_set() {
        let mut shell = Shell::new(TiledLayout::Bsp, Fraction::new(0.5));
        shell.add_output("test");
        assert!(shell.shift_view("test", 1));
        assert_eq!(shell.visible_tags("test"), 0b10);
        assert!(shell.shift_view("test", 1));
        assert_eq!(shell.visible_tags("test"), 0b100);
        assert!(shell.shift_view("test", -1));
        assert_eq!(shell.visible_tags("test"), 0b10);
        assert!(!shell.shift_view("test", -5), "越界不动");
        assert_eq!(shell.visible_tags("test"), 0b10);

        // 多 tag 集合：+1 从最高的开始，-1 从最低的开始。
        shell.set_visible_tags("test", 0b101);
        assert!(shell.shift_view("test", 1));
        assert_eq!(shell.visible_tags("test"), 0b1000);
        shell.set_visible_tags("test", 0b101);
        assert!(!shell.shift_view("test", -1), "最低的已经在 0，向前越界");
    }

    #[test]
    fn shell_starts_with_first_tag_visible() {
        let mut shell = Shell::new(TiledLayout::Bsp, Fraction::new(0.5));
        shell.add_output("test");
        assert_eq!(shell.visible_tags("test"), 1);
        assert_eq!(shell.focused_tag("test"), 1);
        assert!(shell.is_empty());

        // 每个输出有独立的可见 tag。
        shell.add_output("second");
        shell.set_visible_tags("second", 0b10);
        assert_eq!(shell.visible_tags("test"), 1);
        assert_eq!(shell.visible_tags("second"), 0b10);
    }
}

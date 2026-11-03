//! 指针 grab：拖动/缩放浮动窗口。
//!
//! 缩放同时服务两条路径：合成器侧 `super+右键` 拖拽，以及客户端的
//! `xdg_toplevel.resize` 请求（`handlers.rs`）。

use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{IsAlive, Logical, Point, Rectangle, Size},
};

use crate::compositor::state::State;
use crate::shell::Layer;

/// 左键按住标题栏/窗口时的移动 grab。
pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<State>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    /// 从标题条起拖时要等的位移（基线 `movethreshold`）；直接拖动为 0。
    pub threshold: f64,
}

impl PointerGrab<State> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // grab 期间客户端不持有指针焦点。
        handle.motion(data, None, event);

        // 标题条上的轻点仍算点击：没超过阈值前不移动窗口。
        if self.threshold > 0.0 {
            let delta = event.location - self.start_data.location;
            if delta.x * delta.x + delta.y * delta.y < self.threshold * self.threshold {
                return;
            }
        }

        let grabbed = data.shell.id_of_window(&self.window);
        let grabbed = grabbed.and_then(|id| data.shell.find(id).map(|entry| (id, entry.layer)));

        // 平铺窗口不自由移动：拖到哪个格子上，就和那个窗口交换（i3/sway 式）。
        if let Some((id, Layer::Tiled)) = grabbed {
            let target = data
                .space
                .element_under(event.location)
                .map(|(window, _)| window.clone());
            if let Some(target) = target
                && target != self.window
                && let Some(other) = data.shell.id_of_window(&target)
                && data.shell.swap_tiles(id, other)
            {
                data.arrange();
            }
            return;
        }

        let delta = event.location - self.start_data.location;
        let mut new_location = (self.initial_window_location.to_f64() + delta).to_i32_round();

        // 吸附：先按自由位置移动，再贴到工作区/相邻浮动窗口的边缘。
        let snap = data.config.general.move_snap;
        if snap > 0 {
            let size = self.window.geometry().size;
            new_location =
                data.snap_floating(&self.window, Rectangle::new(new_location, size), snap);
        }

        data.space
            .map_element(self.window.clone(), new_location, false);
        #[cfg(feature = "xwayland")]
        data.sync_x11_geometry(
            &self.window,
            Rectangle::new(new_location, self.window.geometry().size),
        );

        // 记住浮动窗口的新位置，重排时不会跳回去。
        // `floating_loc` 是标题条左上角，窗口内容是它的下一条。
        let th = data.title_height();
        if let Some(id) = data.shell.id_of_window(&self.window)
            && let Some(entry) = data.shell.find_mut(id)
            && entry.layer == Layer::Floating
        {
            entry.floating_loc = Some((new_location.x, new_location.y - th));
        }
        data.sync_title_placements();
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        const BTN_LEFT: u32 = 0x110;
        if !handle.current_pressed().contains(&BTN_LEFT) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}

/// 缩放某个窗口的一条/两条边。`initial_*` 是按下时的快照。
pub struct ResizeSurfaceGrab {
    pub start_data: PointerGrabStartData<State>,
    pub window: Window,
    pub edges: xdg_toplevel::ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
    /// 上一次 motion 的累计位移，用来给布局层传增量。
    pub last_delta: Point<f64, Logical>,
}

/// 把协议里的边枚举拆成 (左, 上, 右, 下)。
fn edge_flags(edges: xdg_toplevel::ResizeEdge) -> (bool, bool, bool, bool) {
    use xdg_toplevel::ResizeEdge::*;
    match edges {
        None => (false, false, false, false),
        Top => (false, true, false, false),
        Bottom => (false, false, false, true),
        Left => (true, false, false, false),
        TopLeft => (true, true, false, false),
        BottomLeft => (true, false, false, true),
        Right => (false, false, true, false),
        TopRight => (false, true, true, false),
        BottomRight => (false, false, true, true),
        _ => (false, false, false, false),
    }
}

/// 按拖动增量算出缩放后的 (位置, 尺寸)；拖左/上边时对边固定。
fn resized_geometry(
    initial_location: Point<i32, Logical>,
    initial_size: Size<i32, Logical>,
    edges: xdg_toplevel::ResizeEdge,
    delta: Point<f64, Logical>,
) -> (Point<i32, Logical>, Size<i32, Logical>) {
    let (left, top, right, bottom) = edge_flags(edges);

    let mut width = initial_size.w;
    let mut height = initial_size.h;
    if left {
        width -= delta.x.round() as i32;
    } else if right {
        width += delta.x.round() as i32;
    }
    if top {
        height -= delta.y.round() as i32;
    } else if bottom {
        height += delta.y.round() as i32;
    }
    let size = Size::from((width.max(1), height.max(1)));

    let location = Point::from((
        if left {
            initial_location.x + (initial_size.w - size.w)
        } else {
            initial_location.x
        },
        if top {
            initial_location.y + (initial_size.h - size.h)
        } else {
            initial_location.y
        },
    ));
    (location, size)
}

impl PointerGrab<State> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // grab 期间客户端不持有指针焦点。
        handle.motion(data, None, event);

        if !self.window.alive() {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        let total = event.location - self.start_data.location;
        let step = total - self.last_delta;
        self.last_delta = total;

        // 平铺窗口不转浮动：bsp 改分割比例，scroll 改列宽，然后重新布局。
        if let Some(id) = data.shell.id_of_window(&self.window)
            && data
                .shell
                .find(id)
                .is_some_and(|entry| entry.layer == Layer::Tiled)
        {
            data.resize_tiled_window(id, self.edges, step.x, step.y);
            return;
        }

        let (location, size) = resized_geometry(
            self.initial_window_location,
            self.initial_window_size,
            self.edges,
            total,
        );

        self.last_window_size = size;
        if let Some(toplevel) = self.window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.size = Some(size);
            });
            toplevel.send_pending_configure();
        }
        #[cfg(feature = "xwayland")]
        if let Some(surface) = self.window.x11_surface()
            && !surface.is_override_redirect()
        {
            let _ = surface.configure(Rectangle::new(location, size));
        }

        data.space.map_element(self.window.clone(), location, false);
        let th = data.title_height();
        if let Some(id) = data.shell.id_of_window(&self.window)
            && let Some(entry) = data.shell.find_mut(id)
        {
            entry.floating_loc = Some((location.x, location.y - th));
        }
        data.sync_title_placements();
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().is_empty() {
            return;
        }
        handle.unset_grab(self, data, event.serial, event.time, true);

        // 平铺窗口的尺寸由布局决定，安排已经在 motion 里做完。
        if let Some(id) = data.shell.id_of_window(&self.window)
            && data
                .shell
                .find(id)
                .is_some_and(|entry| entry.layer == Layer::Tiled)
        {
            return;
        }

        if let Some(toplevel) = self.window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(self.last_window_size);
            });
            toplevel.send_pending_configure();
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdg_toplevel::ResizeEdge;

    fn geometry(edges: ResizeEdge, dx: f64, dy: f64) -> (Point<i32, Logical>, Size<i32, Logical>) {
        resized_geometry(
            Point::from((100, 100)),
            Size::from((200, 100)),
            edges,
            Point::from((dx, dy)),
        )
    }

    #[test]
    fn resize_keeps_the_opposite_edge_fixed() {
        let (loc, size) = geometry(ResizeEdge::BottomRight, 50.0, 20.0);
        assert_eq!(
            (loc, size),
            (Point::from((100, 100)), Size::from((250, 120)))
        );

        let (loc, size) = geometry(ResizeEdge::TopLeft, 40.0, 10.0);
        assert_eq!(
            (loc, size),
            (Point::from((140, 110)), Size::from((160, 90)))
        );

        let (loc, size) = geometry(ResizeEdge::Left, 30.0, 0.0);
        assert_eq!(
            (loc, size),
            (Point::from((130, 100)), Size::from((170, 100)))
        );

        let (loc, size) = geometry(ResizeEdge::Top, 0.0, 25.0);
        assert_eq!(
            (loc, size),
            (Point::from((100, 125)), Size::from((200, 75)))
        );
    }

    #[test]
    fn resize_never_goes_below_one_pixel() {
        let (loc, size) = geometry(ResizeEdge::TopLeft, 500.0, 500.0);
        assert_eq!(size, Size::from((1, 1)));
        assert_eq!(loc, Point::from((299, 199)));
    }
}

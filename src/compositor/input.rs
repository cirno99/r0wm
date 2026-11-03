//! 输入事件分发：键盘、指针。
//!
//! 键盘先过绑定表：命中就执行动作并截断事件，否则交给客户端。
//! 指针在 `super` 按下时做「左键拖动 / 右键缩放窗口」，其余情况照常转发。

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        InputTime, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, Focus, MotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};

use super::{
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    state::State,
};
use crate::{action::Action, layout::TiledLayout, ui::bar::BarHit, ui::title::TitleHit};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event, .. } => self.on_pointer_motion::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.on_pointer_motion_absolute::<I>(event)
            }
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: impl KeyboardKeyEvent<I>) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time(&event);
        let keycode = event.key_code();
        let key_state = event.state();

        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        keyboard.input::<(), _>(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| {
                state.keyboard_modifiers = *modifiers;
                // 命令提示打开时独占键盘（基线 `runner_active`），优先级高于选择器。
                if state.runner.active() {
                    if key_state == smithay::backend::input::KeyState::Pressed {
                        state.runner_key(handle.modified_sym(), modifiers);
                    }
                    return FilterResult::Intercept(());
                }
                // 托盘菜单打开时也独占键盘；Esc 收起（基线 `traypopup_dismiss`）。
                if state.tray_popup_active() {
                    if key_state == smithay::backend::input::KeyState::Pressed
                        && handle.modified_sym() == smithay::input::keyboard::Keysym::Escape
                    {
                        state.tray_popup_dismiss();
                    }
                    return FilterResult::Intercept(());
                }
                // 动作选择器打开时它独占键盘（基线 `notifypicking`）。
                if state.notify_pick.is_some() {
                    if key_state == smithay::backend::input::KeyState::Pressed {
                        state.notify_pick_key(&handle.raw_syms());
                    }
                    return FilterResult::Intercept(());
                }
                if key_state != smithay::backend::input::KeyState::Pressed {
                    return FilterResult::Forward;
                }
                // 用未修饰的 keysym 查表，`Shift+h` 才能命中配置里的 `"h"`。
                let action = state.binds.lookup(modifiers, &handle.raw_syms()).cloned();
                match action {
                    Some(action) => {
                        state.execute(action);
                        FilterResult::Intercept(())
                    }
                    None => FilterResult::Forward,
                }
            },
        );
    }

    /// 相对移动（libinput 的鼠标）：夹在输出边界内，再走与绝对定位相同的分发。
    fn on_pointer_motion<I: InputBackend>(&mut self, event: impl PointerMotionEvent<I>) {
        // 指针在整张桌面上移动，不能夹在「当前焦点输出」里；
        // 否则多显示器时鼠标永远过不去第二块屏。
        let bounds: Option<(i32, i32, i32, i32)> = self.outputs.iter().fold(None, |acc, output| {
            let geo = self.space.output_geometry(output)?;
            Some(match acc {
                None => (
                    geo.loc.x,
                    geo.loc.y,
                    geo.loc.x + geo.size.w,
                    geo.loc.y + geo.size.h,
                ),
                Some((x0, y0, x1, y1)) => (
                    x0.min(geo.loc.x),
                    y0.min(geo.loc.y),
                    x1.max(geo.loc.x + geo.size.w),
                    y1.max(geo.loc.y + geo.size.h),
                ),
            })
        });
        let Some((x0, y0, x1, y1)) = bounds else {
            return;
        };
        let delta = event.delta();
        let position = Point::from((
            (self.pointer_location.x + delta.x).clamp(x0 as f64, (x1 - 1) as f64),
            (self.pointer_location.y + delta.y).clamp(y0 as f64, (y1 - 1) as f64),
        ));
        self.dispatch_pointer_motion(position, event.time());
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: impl AbsolutePositionEvent<I>,
    ) {
        let Some(output) = self.output.clone() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let position = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        self.dispatch_pointer_motion(position, event.time());
    }

    /// 指针移到 `position` 之后的公共分发：托盘菜单、标题条悬停、客户端 motion。
    fn dispatch_pointer_motion(&mut self, position: Point<f64, Logical>, time: InputTime) {
        // 光标位置变了就要重画（哪怕客户端不接指针事件）。
        self.request_redraw();
        self.pointer_location = position;
        // 指针进入哪个输出，后续 tag/布局/新建窗口就落在哪个输出。
        let under = self
            .space
            .output_under(position)
            .next()
            .map(|output| output.name());
        if let Some(name) = under
            && self.output.as_ref().map(|current| current.name()) != Some(name.clone())
        {
            self.focus_output(&name);
        }
        // 托盘菜单打开时只用指针更新悬停行，事件不下发（基线 `traypopup_motion`）。
        if self.tray_popup_active() {
            self.tray_popup_motion(position);
            return;
        }
        // 标题条的关闭按钮只在悬停时出现；下一帧重画时生效。
        self.set_title_hover(position);

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under(position);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: position,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: impl PointerButtonEvent<I>) {
        self.request_redraw();
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let button = event.button_code();
        let button_state = event.state();
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time();

        // 托盘菜单打开时它截走所有按键；按在行内选中，按在行外取消。
        if self.tray_popup_active() {
            if button_state == ButtonState::Pressed {
                self.tray_popup_click(self.pointer_location);
            }
            return;
        }

        // 客户端没请求显式 grab 的 popup 也要「点外关闭」（wlroots 的隐式 grab）。
        if button_state == ButtonState::Pressed && !self.popup_contains(self.pointer_location) {
            self.dismiss_all_popups();
        }

        // bar 是合成器自绘的，点击由自己消化，不再转发给客户端；
        // 落在条带空白处的点击也一样（基线把整条 bar 当作输入区）。
        if button_state == ButtonState::Pressed {
            if let Some(hit) = self.bar_hit(self.pointer_location) {
                self.bar_click(hit, button);
                return;
            }
            if self.bar_contains(self.pointer_location) {
                return;
            }
            // 标题条是合成器自绘的叠层：点在它上而不往下传（基线 `titleclick`）。
            if let Some(hit) = self.title_hit(self.pointer_location) {
                self.title_click(hit, button, serial);
                return;
            }
        }

        // layer-shell 的 `OnDemand` 键盘：点它才给焦点，点到别处就归还。
        if button_state == ButtonState::Pressed {
            match self.on_demand_layer_under(self.pointer_location) {
                Some(surface) => self.focus_layer_surface(surface),
                None => {
                    if self.layer_focus_is_on_demand() {
                        self.release_layer_focus();
                    }
                }
            }
        }

        // `super` + 左键拖动 / 右键从角上缩放；指针在 layer 表面（bar）上时不抢窗口。
        if button_state == ButtonState::Pressed
            && matches!(button, BTN_LEFT | BTN_RIGHT | BTN_MIDDLE)
            && !pointer.is_grabbed()
            && self.keyboard_modifiers.logo
            && !self.pointer_over_layer(self.pointer_location)
        {
            let target = self
                .space
                .element_under(self.pointer_location)
                .map(|(window, _)| window.clone());

            if let Some(window) = target {
                if let Some(id) = self.shell.id_of_window(&window) {
                    self.focus_window(Some(id), serial);
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time,
                    },
                );

                if button == BTN_LEFT {
                    // 全屏窗口不拖动；平铺窗口由 grab 内部处理和格子交换。
                    let movable = self
                        .shell
                        .id_of_window(&window)
                        .and_then(|id| self.shell.find(id))
                        .is_some_and(|entry| entry.layer != crate::shell::Layer::Fullscreen);
                    if movable
                        && let Some(start_data) = pointer.grab_start_data()
                        && let Some(initial_window_location) = self.space.element_location(&window)
                    {
                        let grab = MoveSurfaceGrab {
                            start_data,
                            window: window.clone(),
                            initial_window_location,
                            threshold: 0.0,
                        };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                    }
                } else if let Some(id) = self.shell.id_of_window(&window) {
                    // 平铺窗口保持布局，只改分割比例/列宽；浮动窗口自由缩放。
                    if can_resize(self, id)
                        && let Some(start_data) = pointer.grab_start_data()
                        && let Some(initial_window_location) = self.space.element_location(&window)
                    {
                        let initial_window_size = window.geometry().size;
                        let geometry = self
                            .space
                            .element_geometry(&window)
                            .unwrap_or_else(|| window.geometry());
                        let edges = corner_edges(self.pointer_location, geometry);
                        let grab = ResizeSurfaceGrab {
                            start_data,
                            window: window.clone(),
                            edges,
                            initial_window_location,
                            initial_window_size,
                            last_window_size: initial_window_size,
                            last_delta: Default::default(),
                        };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                    }
                }
                pointer.frame(self);
                return;
            }
        }

        if button_state == ButtonState::Pressed
            && !pointer.is_grabbed()
            && !self.pointer_over_layer(self.pointer_location)
        {
            let target = self
                .space
                .element_under(self.pointer_location)
                .map(|(window, _)| window.clone());

            match target {
                Some(window) => {
                    let id = self.shell.id_of_window(&window);
                    self.focus_window(id, serial);
                }
                None => self.focus_window(None, serial),
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// 标题条点击：无修饰左键在关闭按钮上就关窗口，其余左键开始拖动
    /// （基线 `titleclick` → `client_send_close` / `titlegrab`）。
    fn title_click(&mut self, hit: TitleHit, button: u32, serial: smithay::utils::Serial) {
        if button != BTN_LEFT {
            return;
        }
        let mods = self.keyboard_modifiers;
        let no_mods = !mods.logo && !mods.ctrl && !mods.alt && !mods.shift;
        if hit.close && no_mods {
            self.close_window(hit.id);
            return;
        }
        if no_mods || mods.logo {
            self.start_title_drag(hit.id, serial);
        }
    }

    /// 点住标题条拖动：先聚焦（tab 组会把该窗口提到栈顶），再交给移动 grab。
    fn start_title_drag(&mut self, id: crate::shell::WindowId, serial: smithay::utils::Serial) {
        use crate::shell::Layer;

        if self
            .shell
            .find(id)
            .is_none_or(|entry| entry.layer == Layer::Fullscreen)
        {
            return;
        }
        self.focus_window(Some(id), serial);

        let Some(window) = self.shell.window(id).cloned() else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };
        let threshold = self.config.general.move_threshold;
        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            threshold,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// bar 点击（基线默认鼠标绑定）：tag 上左键 `view`、右键 `toggle_view`、
    /// `super`+左键 `transfer_to`、`super`+右键 `toggle_tag`；布局名循环切换；
    /// 状态文本按左/右/中键触发 `^sN!命令^` 的点击区。
    fn bar_click(&mut self, hit: BarHit, button: u32) {
        match hit {
            BarHit::Tag(mask) => {
                let action = match (self.keyboard_modifiers.logo, button) {
                    (false, BTN_LEFT) => Action::View(mask),
                    (false, BTN_RIGHT) => Action::ToggleView(mask),
                    (true, BTN_LEFT) => Action::TransferTo(mask),
                    (true, BTN_RIGHT) => Action::ToggleTag(mask),
                    _ => return,
                };
                self.execute(action);
            }
            BarHit::Layout => {
                let output = self.focused_output_name();
                let current = self
                    .shell
                    .layout_of(&output, self.shell.focused_tag(&output));
                let next = match current {
                    TiledLayout::Bsp => TiledLayout::Scroll,
                    TiledLayout::Scroll => TiledLayout::Tabbed,
                    TiledLayout::Tabbed => TiledLayout::Bsp,
                };
                self.execute(Action::SetLayout(next));
            }
            BarHit::Status => {
                let number = match button {
                    BTN_LEFT => 1,
                    BTN_RIGHT => 2,
                    BTN_MIDDLE => 3,
                    _ => 0,
                };
                if number > 0 {
                    self.status_click(self.pointer_location, number);
                }
            }
            BarHit::Notify => match button {
                BTN_LEFT => self.notify_open_click(),
                BTN_RIGHT => self.notify_dismiss(),
                _ => {}
            },
            BarHit::NotifySign => {
                if button == BTN_LEFT {
                    self.notify_actions();
                }
            }
            BarHit::NotifyAction(index) => {
                if button == BTN_LEFT {
                    self.notify_pick_run(index);
                }
            }
            BarHit::Tray(index) => match button {
                BTN_LEFT => self.tray_activate(index),
                BTN_RIGHT => self.tray_menu(index),
                _ => {}
            },
        }
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: impl PointerAxisEvent<I>) {
        self.request_redraw();
        let source = event.source();

        let horizontal = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
        let vertical = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);
        let horizontal_v120 = event.amount_v120(Axis::Horizontal);
        let vertical_v120 = event.amount_v120(Axis::Vertical);

        // 指针在 bar 上：tag 区的滚轮前后切 tag，状态文本的滚轮给点击区，
        // 通知区的纵向滚轮走历史、横向滚正文；都不再转发给客户端（基线行为）。
        if self.bar_contains(self.pointer_location) {
            let hit = self.bar_hit(self.pointer_location);
            if matches!(
                hit,
                Some(BarHit::Notify | BarHit::NotifySign | BarHit::NotifyAction(_))
            ) && (vertical != 0.0 || horizontal != 0.0)
            {
                self.notify_wheel(vertical, horizontal);
                return;
            }
            if vertical != 0.0 {
                match hit {
                    Some(BarHit::Tag(_)) => {
                        let step = if vertical < 0.0 { -1 } else { 1 };
                        self.execute(Action::ShiftView(step));
                    }
                    _ => {
                        self.status_scroll(self.pointer_location, vertical);
                    }
                }
                return;
            }
        }

        // 指针在标题条上：基线 `ClkWinTitle` 的滚轮绑定是 `focusstack ±1`。
        if vertical != 0.0 && self.title_hit(self.pointer_location).is_some() {
            self.execute(if vertical < 0.0 {
                Action::FocusPrev
            } else {
                Action::FocusNext
            });
            return;
        }

        let mut frame = AxisFrame::new(event.time()).source(source);
        if horizontal != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal);
            if let Some(discrete) = horizontal_v120 {
                frame = frame.v120(Axis::Horizontal, discrete as i32);
            }
        }
        if vertical != 0.0 {
            frame = frame.value(Axis::Vertical, vertical);
            if let Some(discrete) = vertical_v120 {
                frame = frame.v120(Axis::Vertical, discrete as i32);
            }
        }
        if source == AxisSource::Finger {
            if event.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if event.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// 键盘/指针焦点一起落到某个窗口上；`None` 表示清空焦点。
    /// 具体映射、滚动与激活态由 [`State::arrange`] 统一处理。
    pub fn focus_window(
        &mut self,
        id: Option<crate::shell::WindowId>,
        _serial: smithay::utils::Serial,
    ) {
        match id {
            Some(id) if self.shell.find(id).is_some() => {
                if let Some(entry) = self.shell.find(id) {
                    let output = entry.output.clone();
                    self.focus_output(&output);
                }
                self.shell.set_focus(id);
            }
            _ => self.shell.clear_focus(),
        }
        let output = self.focused_output_name();
        let area = self.focused_work_area();
        self.shell.note_focus(&output, area);
        self.arrange();
    }
}

/// 按指针落在窗口的哪个象限，挑一个角作为缩放边。
fn corner_edges(
    pointer: Point<f64, Logical>,
    geometry: Rectangle<i32, Logical>,
) -> xdg_toplevel::ResizeEdge {
    use xdg_toplevel::ResizeEdge::*;
    let left = pointer.x < f64::from(geometry.loc.x + geometry.size.w / 2);
    let top = pointer.y < f64::from(geometry.loc.y + geometry.size.h / 2);
    match (left, top) {
        (true, true) => TopLeft,
        (false, true) => TopRight,
        (true, false) => BottomLeft,
        (false, false) => BottomRight,
    }
}

/// 全屏窗口不参与交互缩放；平铺/浮动窗口都允许。
fn can_resize(state: &State, id: crate::shell::WindowId) -> bool {
    matches!(
        state.shell.find(id).map(|entry| entry.layer),
        Some(crate::shell::Layer::Tiled | crate::shell::Layer::Floating)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    #[test]
    fn corner_edges_follow_the_pointer_quadrant() {
        let geometry = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        let at = |x: f64, y: f64| corner_edges(Point::from((x, y)), geometry);

        assert_eq!(at(120.0, 120.0), xdg_toplevel::ResizeEdge::TopLeft);
        assert_eq!(at(280.0, 120.0), xdg_toplevel::ResizeEdge::TopRight);
        assert_eq!(at(120.0, 180.0), xdg_toplevel::ResizeEdge::BottomLeft);
        assert_eq!(at(280.0, 180.0), xdg_toplevel::ResizeEdge::BottomRight);
    }
}

//! XWayland 集成：启动 X server、把 X11 窗口接进现有的 Shell/Space。
//!
//! r0wm 的窗口层（tag、布局、标题条）都围绕 `smithay::desktop::Window` 建的；
//! Smithay 的 `Window` 本身已经能包 X11Surface，所以这里只需要把 XwmHandler
//! 收到的事件翻译成 Shell 的增删改，不需要再复制一套窗口模型。

use std::process::Stdio;

use super::{
    focus::KeyboardFocusTarget,
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    state::{State, X11Selection},
};
use crate::shell::WindowId;
use smithay::{
    desktop::Window,
    input::pointer::Focus,
    reexports::{
        calloop::EventLoop,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_keyboard_grab::XWaylandKeyboardGrabHandler,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{ResizeEdge, WmWindowProperty, XwmId},
    },
};

/// 在事件循环里启动 XWayland。返回 display number，失败时 `None`。
///
/// `DISPLAY` 在这里立刻设置：等 `Ready` 就太晚了——session 启动脚本和
/// 快捷键拉起的 X11 客户端都会在这之前读环境变量。
pub fn start(event_loop: &mut EventLoop<'static, State>, dh: &DisplayHandle) -> Option<u32> {
    let (xwayland, client) = match XWayland::spawn(
        dh,
        None,
        std::iter::empty::<(String, String)>(),
        std::iter::empty::<String>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    ) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!("启动 XWayland 失败，X11 应用不可用：{err}");
            return None;
        }
    };

    let display_number = xwayland.display_number();
    // 2024 edition 里 set_var 是 unsafe：这里发生在合成器启动早期，
    // 还没有拉起可能并发读取环境的线程。
    unsafe {
        std::env::set_var("DISPLAY", format!(":{display_number}"));
    }
    tracing::info!(display = display_number, "XWayland 已启动");

    let handle = event_loop.handle();
    let dh = dh.clone();
    let inserted = event_loop
        .handle()
        .insert_source(xwayland, move |event, _, state| {
            match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    match X11Wm::start_wm(handle.clone(), &dh, x11_socket, client.clone()) {
                        Ok(wm) => {
                            // 不要在这里调用 `wm.set_randr_primary_output(...)`：
                            // 它会在主线程上读同一条 x11rb 连接，与 X11Source 的
                            // 事件线程争用；实测之后 MapRequest 全部丢失，X11
                            // 窗口永远停在 UnMapped。XWayland 默认主输出够用。
                            state.xwm = Some(wm);
                            state.xdisplay = Some(display_number);
                            tracing::info!(display = display_number, "X11 window manager 就绪");
                        }
                        Err(err) => tracing::error!("启动 X11 window manager 失败：{err}"),
                    }
                }
                XWaylandEvent::Error => tracing::error!("XWayland 启动过程中退出"),
            }
        });
    if let Err(err) = inserted {
        tracing::error!("注册 XWayland 事件源失败：{err}");
        return None;
    }

    Some(display_number)
}

/// 按 X11 window id 找已经接管的窗口。

fn find_managed_id(state: &State, surface: &X11Surface) -> Option<WindowId> {
    state
        .shell
        .iter()
        .find(|entry| {
            entry
                .window
                .x11_surface()
                .is_some_and(|candidate| candidate.window_id() == surface.window_id())
        })
        .map(|entry| entry.id)
}

fn find_override(state: &State, surface: &X11Surface) -> Option<Window> {
    state
        .x11_override
        .iter()
        .find(|window| {
            window
                .x11_surface()
                .is_some_and(|candidate| candidate.window_id() == surface.window_id())
        })
        .cloned()
}

impl State {
    fn map_x11_window(&mut self, surface: X11Surface) {
        if find_managed_id(self, &surface).is_some() {
            let _ = surface.set_mapped(true);
            self.arrange();
            return;
        }

        let window = Window::new_x11_window(surface.clone());
        let (layer, tag, output) = self.default_placement();
        let id = self.shell.add(window.clone(), layer, tag, &output);
        if let Some(entry) = self.shell.find_mut(id) {
            entry.placed = true;
            entry.title = Some(surface.title()).filter(|title| !title.is_empty());
            entry.app_id = Some(surface.class()).filter(|class| !class.is_empty());
        }
        if let Err(err) = surface.set_mapped(true) {
            tracing::warn!(id, "映射 X11 窗口失败：{err}");
        }
        tracing::info!(id, %output, "新 X11 顶层窗口");
        self.arrange();
    }

    fn map_x11_override(&mut self, surface: X11Surface) {
        if find_override(self, &surface).is_some() {
            return;
        }
        let window = Window::new_x11_window(surface.clone());
        let location = surface.geometry().loc;
        self.space.map_element(window.clone(), location, false);
        self.space.raise_element(&window, false);
        self.x11_override.push(window);
        tracing::debug!("映射 X11 override-redirect 窗口");
    }

    fn forget_x11_window(&mut self, surface: &X11Surface) {
        if let Some(id) = find_managed_id(self, surface)
            && let Some(window) = self.shell.window(id).cloned()
        {
            self.space.unmap_elem(&window);
            self.shell.remove_by_id(id);
        }
        if let Some(window) = find_override(self, surface) {
            self.space.unmap_elem(&window);
        }
        self.x11_override.retain(|window| {
            window
                .x11_surface()
                .is_some_and(|candidate| candidate.window_id() != surface.window_id())
        });
        self.arrange();
    }

    fn refresh_x11_properties(&mut self, surface: &X11Surface) {
        let Some(id) = find_managed_id(self, surface) else {
            return;
        };
        let title = Some(surface.title()).filter(|title| !title.is_empty());
        let app_id = Some(surface.class()).filter(|class| !class.is_empty());
        self.set_window_title(id, title);
        self.set_window_app_id(id, app_id);
        self.update_titles_for_all();
    }

    fn update_titles_for_all(&mut self) {
        let outputs = self.outputs.clone();
        for output in outputs {
            self.update_titles(&output);
        }
    }

    fn focus_x11(&mut self, surface: &X11Surface) {
        let Some(id) = find_managed_id(self, surface) else {
            return;
        };
        self.shell.set_focus(id);
        self.apply_focus(Some(id), SERIAL_COUNTER.next_serial());
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11Wm 回调早于 start_wm")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "X11 new_window");
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.map_x11_window(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.map_x11_override(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<smithay::xwayland::xwm::Reorder>,
    ) {
        if find_managed_id(self, &window).is_some() {
            // 平铺窗口的几何由布局决定；浮动窗口也先按 r0wm 自己的位置走。
            self.arrange();
            return;
        }

        let current = window.last_configure();
        let rect = Rectangle::new(
            Point::from((x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y))),
            Size::from((
                w.map(|value| value.max(1) as i32).unwrap_or(current.size.w),
                h.map(|value| value.max(1) as i32).unwrap_or(current.size.h),
            )),
        );
        if let Err(err) = window.configure(rect) {
            tracing::debug!("X11 未映射窗口 configure 失败：{err}");
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // override-redirect 窗口自己决定几何；托管窗口的几何由 arrange 下发。
        if let Some(element) = find_override(self, &window) {
            self.space.map_element(element.clone(), geometry.loc, false);
            self.space.raise_element(&element, false);
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class) {
            self.refresh_x11_properties(&window);
        }
    }

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _timestamp: u32,
        _currently_active_window: Option<X11Surface>,
    ) {
        self.focus_x11(&window);
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edges: ResizeEdge) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(id) = find_managed_id(self, &window) else {
            return;
        };
        let Some(element) = self.shell.window(id).cloned() else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&element) else {
            return;
        };
        let initial_window_size = element.geometry().size;
        let edges = match edges {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
        };
        let grab = ResizeSurfaceGrab {
            start_data,
            window: element,
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            last_delta: Default::default(),
        };
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(id) = find_managed_id(self, &window) else {
            return;
        };
        let Some(element) = self.shell.window(id).cloned() else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&element) else {
            return;
        };
        self.float_window(id);
        let grab = MoveSurfaceGrab {
            start_data,
            window: element,
            initial_window_location,
            threshold: 0.0,
        };
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        // 个人桌面不区分客户端信任级：X11 程序可以直接读 Wayland 侧选区。
        true
    }

    /// X11 程序要读 Wayland 选区：把请求转给当前持有选区的 Wayland 客户端。
    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        let result = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd)
                    .map_err(|err| format!("{err:?}"))
            }
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|err| format!("{err:?}")),
        };
        if let Err(err) = result {
            tracing::warn!(?err, ?selection, "把 Wayland 选区送给 X11 客户端失败");
        }
    }

    /// X11 程序接管了选区：把 X11 的 mime 列表当成服务端选区发布给 Wayland。
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        tracing::debug!(?selection, ?mime_types, "X11 设置选区");
        let dh = &self.display_handle;
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(dh, &self.seat, mime_types, X11Selection)
            }
            SelectionTarget::Primary => {
                set_primary_selection(dh, &self.seat, mime_types, X11Selection)
            }
        }
    }

    /// X11 侧清掉了选区：只有当前服务端选区也是 X11 的才跟着清，
    /// 否则会把刚由 Wayland 客户端设好的选区误删。
    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        let dh = &self.display_handle;
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(dh, &self.seat);
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.xdisplay = None;
        tracing::warn!("X11 window manager 连接断开");
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl XWaylandKeyboardGrabHandler for State {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<Self::KeyboardFocus> {
        let x11 = self
            .shell
            .iter()
            .find_map(|entry| {
                entry
                    .window
                    .x11_surface()
                    .filter(|candidate| candidate.wl_surface().as_ref() == Some(surface))
                    .cloned()
            })
            .or_else(|| {
                self.x11_override.iter().find_map(|window| {
                    window
                        .x11_surface()
                        .filter(|candidate| candidate.wl_surface().as_ref() == Some(surface))
                        .cloned()
                })
            })?;
        Some(KeyboardFocusTarget::X11(x11))
    }
}

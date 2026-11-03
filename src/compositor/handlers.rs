//! Smithay 各协议 handler 的实现。
//!
//! M1 只需要「能开窗、能画、能点」：compositor / shm / xdg-shell / seat /
//! data-device / output。其余协议按 `docs/protocols.md` 的 v1 清单在 M3 起补齐。

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    backend::input::{Event, InputEvent, KeyboardKeyEvent},
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::{
        LayerSurface, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, Window,
        WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output,
    },
    input::SeatHandler,
    input::{
        Seat,
        dnd::{DnDGrab, DndGrabHandler, GrabType, Source},
        keyboard::FilterResult,
        pointer::{Focus, GrabStartData as PointerGrabStartData},
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Rectangle, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        input_method::{
            InputMethodHandler, InputMethodSeat, PopupSurface as InputMethodPopupSurface,
        },
        output::OutputHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer as WlrLayer, LayerSurface as WlrLayerSurface,
                LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
            },
        },
        shm::{ShmHandler, ShmState},
        text_input::TextInputActivation,
        virtual_keyboard::{
            VirtualKeyboardBackend, VirtualKeyboardHandler, VirtualKeyboardSpecialEvent,
        },
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};

#[cfg(any(feature = "winit", feature = "udev"))]
use smithay::backend::renderer::ImportDma;

use super::{
    focus::KeyboardFocusTarget,
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    state::{ClientState, State},
};
use crate::shell::Layer;

#[cfg(feature = "xwayland")]
use smithay::xwayland::XWaylandClientData;

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.try_import_dmabuf(&dmabuf) {
            if let Err(err) = notifier.successful::<State>() {
                tracing::warn!("dmabuf 导入成功但回执失败：{err}");
            }
        } else {
            notifier.failed();
        }
    }
}

/// 客户端刚建 fractional-scale 对象时立刻告知当前缩放；之后缩放变化再广播。
impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.send_fractional_scale(&surface);
    }
}

impl State {
    /// 交给后端渲染器试导入；没有渲染器（winit 尚未初始化）时一律失败。
    fn try_import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        #[cfg(feature = "winit")]
        {
            if let Some(data) = self.winit_data.as_mut() {
                return data.backend.renderer().import_dmabuf(dmabuf, None).is_ok();
            }
        }
        #[cfg(feature = "udev")]
        {
            if let Some(renderer) = self.udev_data.as_mut().and_then(|data| data.renderer_mut()) {
                return renderer.import_dmabuf(dmabuf, None).is_ok();
            }
        }
        let _ = dmabuf;
        false
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        #[cfg(feature = "xwayland")]
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        tracing::trace!(id = surface.id().protocol_id(), "surface 提交");
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            // 子 surface 的提交要归到顶层窗口，否则 `on_commit` 永远不被调用。
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }

            let window = self
                .shell
                .id_of_surface(&root)
                .and_then(|id| self.shell.window(id).cloned());

            if let Some(window) = window {
                window.on_commit();
                self.send_initial_configure(&root);
                self.place_if_needed(&root);
            }

            // layer-shell 的初始 configure 必须在首次 commit 后发，且要先根据
            // 客户端提交的尺寸重新 arrange。
            let layer_output = self
                .outputs
                .iter()
                .find(|output| {
                    layer_map_for_output(output)
                        .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                        .is_some()
                })
                .cloned();
            if let Some(output) = layer_output {
                self.commit_layer_surface(&output, &root);
            }
        }

        self.commit_popups(surface);
        // 任何 surface 提交都可能改变画面（内容、尺寸、光标）。
        self.request_redraw();
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let (layer, tag, output) = self.default_placement();
        let id = self.shell.add(window, layer, tag, &output);
        tracing::info!(id, ?layer, %output, "新顶层窗口");

        // 初始 configure 必须发出去，客户端才会提交 buffer。
        // 真实几何（平铺尺寸或浮动位置）等首帧知道尺寸后再定。
        if let Some(toplevel) = self.shell.window(id).and_then(|w| w.toplevel()) {
            toplevel.send_configure();
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("跟踪 popup 失败：{err:?}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let wl_surface = surface.wl_surface().clone();
        let Some(start_data) = check_grab(&seat, &wl_surface, serial) else {
            return;
        };

        let Some(window) = self
            .shell
            .id_of_surface(&wl_surface)
            .and_then(|id| self.shell.window(id).cloned())
        else {
            return;
        };
        let Some(initial_location) = self.space.element_location(&window) else {
            return;
        };

        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location: initial_location,
            threshold: 0.0,
        };
        if let Some(pointer) = seat.get_pointer() {
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let wl_surface = surface.wl_surface().clone();
        let Some(start_data) = check_grab(&seat, &wl_surface, serial) else {
            return;
        };
        let Some(window) = self
            .shell
            .id_of_surface(&wl_surface)
            .and_then(|id| self.shell.window(id).cloned())
        else {
            return;
        };
        let Some(id) = self.shell.id_of_window(&window) else {
            return;
        };
        // 全屏窗口不参与交互缩放；平铺窗口保持原布局，缩放交给布局层。
        if self
            .shell
            .find(id)
            .is_some_and(|entry| entry.layer == Layer::Fullscreen)
        {
            return;
        }
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };
        let initial_window_size = window.geometry().size;

        let grab = ResizeSurfaceGrab {
            start_data,
            window,
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            last_delta: Default::default(),
        };
        if let Some(pointer) = seat.get_pointer() {
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        // 客户端（菜单、输入框下拉）请求显式 grab：指针/键盘都锁在 popup 上，
        // 点到别处由 Smithay 的 PopupPointerGrab 负责 dismiss。
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(surface.clone())) else {
            return;
        };

        match self.popups.grab_popup(
            KeyboardFocusTarget::Wayland(root),
            PopupKind::Xdg(surface),
            &seat,
            serial,
        ) {
            Ok(grab) => {
                if let Some(pointer) = seat.get_pointer() {
                    pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Clear);
                }
                if let Some(keyboard) = seat.get_keyboard() {
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
            }
            Err(err) => tracing::warn!(?err, "popup grab 被拒绝"),
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // M1 没有最大化语义，直接回一个不带尺寸的 configure，避免客户端卡住。
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        surface.send_configure();
    }

    fn show_window_menu(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _location: smithay::utils::Point<i32, smithay::utils::Logical>,
    ) {
        // 窗口菜单随 bar/runner 一起在 M4 之后做。
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let Some(id) = self.shell.id_of_surface(&wl_surface) else {
            return;
        };
        if let Some(window) = self.shell.window(id).cloned() {
            self.space.unmap_elem(&window);
        }
        self.shell.remove_by_id(id);
        tracing::info!(id, "顶层窗口关闭");
        self.arrange();
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.shell.id_of_surface(surface.wl_surface()) else {
            return;
        };
        let title = toplevel_title(&surface);
        self.set_window_title(id, title);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.shell.id_of_surface(surface.wl_surface()) else {
            return;
        };
        let app_id = toplevel_app_id(&surface);
        self.set_window_app_id(id, app_id);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut smithay::input::SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // 命名光标要按输出的缩放挑尺寸；自绘光标的热点在渲染时再取。
        let scale = self
            .output
            .as_ref()
            .map(|output| output.current_scale().integer_scale().max(1) as u32)
            .unwrap_or(1);
        self.pointer_element.set_status(image.clone(), scale);
        self.cursor_status = image;
        self.request_redraw();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
        let display_handle = &self.display_handle;
        let client = focused
            .and_then(|focus| focus.wl_surface())
            .and_then(|surface| display_handle.get_client(surface.id()).ok());
        set_primary_focus(display_handle, seat, client.clone());
        set_data_device_focus(display_handle, seat, client); // 标题条/bar 的高亮跟着焦点走。
        self.request_redraw();
    }
}

impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for State {}

/// 固定协商服务端装饰：标题条由合成器自绘（基线行为），客户端不再画 CSD 标题栏。
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.set_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        self.set_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_decoration(&toplevel);
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // 过期的 token 不再激活，避免陈旧的启动请求抢焦点。
        if token_data.timestamp.elapsed() > std::time::Duration::from_secs(10) {
            tracing::debug!("忽略过期的激活请求");
            return;
        }

        let Some(id) = self.shell.id_of_surface(&surface) else {
            return;
        };
        let Some(entry) = self.shell.find(id) else {
            return;
        };
        let tags = entry.tags;
        let output = entry.output.clone();
        self.focus_output(&output);
        if tags & self.shell.visible_tags(&output) == 0 {
            self.shell.set_visible_tags(&output, tags);
        }
        self.shell.set_focus(id);
        let area = self.focused_work_area();
        self.shell.note_focus(&output, area);
        self.arrange();
    }
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<wl_output::WlOutput>,
        _layer: WlrLayer,
        namespace: String,
    ) {
        let output = output
            .and_then(|resource| Output::from_resource(&resource))
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            tracing::warn!("layer surface 没有可用输出");
            return;
        };

        let layer = LayerSurface::new(surface.clone(), namespace);
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&layer) {
            tracing::warn!("映射 layer surface 失败：{err:?}");
            return;
        }
        drop(map);

        // 这里拿不到最终的 keyboard_interactivity：客户端通常在 get_layer_surface
        // 之后、首次 commit 之前才发 set_keyboard_interactivity，此刻 current
        // state 还是默认的 None。真正的焦点在 commit_layer_surface 里补。

        // exclusive zone 变了，工作区要跟着变（bar 在 M4 走同一条路）。
        self.refresh_work_area();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let output = self
            .outputs
            .iter()
            .find(|output| {
                layer_map_for_output(output)
                    .layers()
                    .any(|layer| layer.layer_surface() == &surface)
            })
            .cloned()
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            return;
        };

        let layer = {
            let map = layer_map_for_output(&output);
            map.layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned()
        };
        if let Some(layer) = layer {
            let surface = layer.wl_surface().clone();
            layer_map_for_output(&output).unmap_layer(&layer);
            // 键盘还在这块 layer 上时要把焦点还给窗口，否则键盘永远没有接收者。
            if self.layer_keyboard_focus.as_ref() == Some(&surface) {
                self.release_layer_focus();
            }
        }
        self.refresh_work_area();
    }
}

/// 输入法（fcitx5/ibus）的预编辑窗口：走与 xdg_popup 相同的 popup 管理，
/// 这样定位、约束与渲染都不需要第二套代码。
impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: InputMethodPopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!("跟踪输入法 popup 失败：{err:?}");
        }
    }

    fn dismiss_popup(&mut self, surface: InputMethodPopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
    }

    fn popup_repositioned(&mut self, _surface: InputMethodPopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        self.space
            .elements()
            .find_map(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == parent)
                    .then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

/// text-input 的激活只用于通知合成器（比如切换输入法状态），这里不需要额外动作：
/// Smithay 已经把 enter/leave 跟键盘焦点绑在一起。
impl TextInputActivation for State {}

/// 虚拟键盘（输入法兜底）：按键只转发给当前焦点客户端，不经过合成器的键绑定，
/// 否则 fcitx 注入的 `Ctrl+Q` 会误触 r0wm 的动作。
impl VirtualKeyboardHandler for State {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        match event {
            InputEvent::DeviceAdded { device }
            | InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                let Some(keymap) = device.keymap() else {
                    return;
                };
                if let Some(keyboard) = self.seat.get_keyboard()
                    && let Err(err) = keyboard.set_keymap_from_keymap_file(self, keymap)
                {
                    tracing::warn!("虚拟键盘 keymap 应用失败：{err:?}");
                }
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.with_xkb_state(self, |mut context| {
                        context.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group)
                    });
                }
            }
            InputEvent::Keyboard { event } => {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                let keycode = event.key_code();
                let key_state = event.state();
                let time = event.time();
                let device = event.device();
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                // 输入法（fcitx5/ibus）会抓住键盘，再把不处理的按键通过自己的
                // 虚拟键盘转发回来。这些键**不能**再走一遍 grab，否则会原路回到
                // 输入法，客户端永远收不到字符。
                let grab_client = self.seat.input_method().keyboard_grab_client();
                if grab_client.is_some() && device.client() == grab_client {
                    // 这里也**不能**先调 `input_intercept`：物理键仍被 MAIN
                    // source 按住，smithay 的跨 source 去重会把这条转发键当成
                    // “已按下”吸收掉，客户端最后只收到 release，表现就是打不出字
                    // 或出现乱码。按上游文档直接绕过 grab 转发，修饰键状态由
                    // `mods_changed = true` 重新同步。
                    keyboard
                        .input_forward_bypassing_grab(self, keycode, key_state, serial, time, true);
                } else {
                    // 普通虚拟键盘（wtype 等）：按设备来源跟踪按键，避免与
                    // 物理键盘上的同一个键互相提前释放。
                    keyboard.input_from_source::<(), _>(
                        device.source(),
                        self,
                        keycode,
                        key_state,
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
            _ => {}
        }
    }
}

impl SelectionHandler for State {
    type SelectionUserData = crate::compositor::state::X11Selection;

    /// Wayland 客户端接管了选区，同步告诉 XWayland，让 X11 程序也能贴。
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        #[cfg(feature = "xwayland")]
        if let Some(xwm) = self.xwm.as_mut() {
            let mime_types = source.map(|source| source.mime_types());
            if let Err(err) = xwm.new_selection(ty, mime_types) {
                tracing::warn!(?err, ?ty, "把选区同步给 XWayland 失败");
            }
        }
        #[cfg(not(feature = "xwayland"))]
        let _ = (ty, source);
    }

    /// Wayland 客户端要读服务端选区：当前只可能是 X11 提供的，去 X server 要。
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
        #[cfg(feature = "xwayland")]
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(ty, mime_type, fd)
        {
            tracing::warn!(?err, "从 X11 读取选区内容失败");
        }
        #[cfg(not(feature = "xwayland"))]
        let _ = (ty, mime_type, fd);
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for State {}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    return;
                };
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => source.cancel(),
        }
    }
}

impl State {
    /// 首帧提交前补发一次带尺寸的 configure（平铺尺寸由 M2 的布局决定）。
    fn send_initial_configure(&self, surface: &WlSurface) {
        let Some(id) = self.shell.id_of_surface(surface) else {
            return;
        };
        let Some(entry) = self.shell.find(id) else {
            return;
        };
        if entry.placed {
            return;
        }
        let Some(toplevel) = entry.window.toplevel() else {
            return;
        };
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            toplevel.send_configure();
        }
    }

    /// 窗口第一次拿到真实尺寸后进入布局。
    fn place_if_needed(&mut self, surface: &WlSurface) {
        let Some(id) = self.shell.id_of_surface(surface) else {
            return;
        };
        let Some(entry) = self.shell.find(id) else {
            return;
        };
        if entry.placed {
            return;
        }
        tracing::debug!(id, layer = ?entry.layer, "窗口首帧就位");
        if entry.layer == Layer::Floating {
            self.place_floating(id);
        } else if let Some(entry) = self.shell.find_mut(id) {
            entry.placed = true;
        }
        self.arrange();
    }

    fn commit_popups(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);
        let Some(popup) = self.popups.find_popup(surface) else {
            return;
        };
        if let PopupKind::Xdg(ref xdg) = popup
            && !xdg.is_initial_configure_sent()
            && let Err(err) = xdg.send_configure()
        {
            tracing::warn!("popup 初始 configure 失败：{err:?}");
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        // popup 的根窗口/layer 可能不在「当前焦点输出」上。
        let entry = self.shell.iter().find(|entry| {
            entry
                .window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &root)
        });
        let output = entry
            .and_then(|entry| {
                self.outputs
                    .iter()
                    .find(|output| output.name() == entry.output)
                    .cloned()
            })
            .or_else(|| {
                self.outputs
                    .iter()
                    .find(|output| {
                        smithay::desktop::layer_map_for_output(output)
                            .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                            .is_some()
                    })
                    .cloned()
            })
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let mut target: Rectangle<i32, smithay::utils::Logical> = output_geo;
        let window = entry.map(|entry| entry.window.clone());

        if let Some(window) = window {
            let Some(window_geo) = self.space.element_geometry(&window) else {
                return;
            };
            target.loc -= window_geo.loc;
        } else {
            // layer-shell 上的 popup（面板菜单）：目标区域是输出减去 layer 自身的位置。
            // 没有这一步，靠近屏幕边缘的菜单会被约束到 (0,0)。
            let map = smithay::desktop::layer_map_for_output(&output);
            let Some(layer) = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL) else {
                return;
            };
            let Some(layer_geo) = map.layer_geometry(layer) else {
                return;
            };
            target.loc -= layer_geo.loc;
        }

        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
    fn commit_layer_surface(&mut self, output: &Output, surface: &WlSurface) {
        let initial_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });

        let mut map = layer_map_for_output(output);
        map.arrange();
        let layer = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned();
        if !initial_sent && let Some(layer) = layer.as_ref() {
            layer.layer_surface().send_configure();
        }
        drop(map);

        // keyboard_interactivity 是双缓冲状态：rofi 等菜单在首次 commit 时才让
        // 它生效。独占键盘的 layer 必须在这里拿焦点，否则键盘事件永远进不了
        // rofi，表现就是方向键无效、Esc 也退不掉（指针事件不受影响）。
        if let Some(layer) = layer {
            let surface = layer.wl_surface().clone();
            let interactivity = layer.cached_state().keyboard_interactivity;
            match interactivity {
                KeyboardInteractivity::Exclusive => {
                    if self.layer_keyboard_focus.as_ref() != Some(&surface) {
                        self.focus_layer_surface(surface);
                    }
                }
                KeyboardInteractivity::None => {
                    if self.layer_keyboard_focus.as_ref() == Some(&surface) {
                        self.release_layer_focus();
                    }
                }
                // OnDemand 的焦点由点击建立，提交内容时不能把它收走。
                KeyboardInteractivity::OnDemand => {}
            }
        }

        self.refresh_work_area();
    }

    /// 与服务端标题条相关的协商：当前固定客户端自绘。
    fn set_decoration(&self, toplevel: &ToplevelSurface) {
        // 基线固定回 SERVER_SIDE（`src/client.c` 的 decoration 初始化），标题条
        // 由合成器自绘；客户端因此不再画自己的 CSD 标题栏。
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

fn toplevel_title(toplevel: &ToplevelSurface) -> Option<String> {
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .and_then(|data| data.title.clone())
    })
}

fn toplevel_app_id(toplevel: &ToplevelSurface) -> Option<String> {
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .and_then(|data| data.app_id.clone())
    })
}

fn check_grab(
    seat: &Seat<State>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<State>> {
    let pointer = seat.get_pointer()?;
    if !pointer.has_grab(serial) {
        return None;
    }
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }
    Some(start_data)
}

smithay::delegate_dispatch2!(State);

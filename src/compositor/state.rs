//! 合成器状态：所有 handler 共享的那一份数据。
//!
//! 结构与 Smithay 的 `smallvil` 示例同构，但窗口不直接进 `Space` 了事：
//! `Shell` 记层/tag/焦点，`Space` 只负责坐标、层叠与命中测试。

use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc, time::Instant};

use smithay::{
    desktop::{PopupManager, Space, Window},
    input::{
        Seat, SeatState,
        keyboard::{Keysym, ModifiersState, XkbConfig},
        pointer::CursorImageStatus,
    },
    output::Output,
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Size, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::{FractionalScaleManagerState, with_fractional_scale},
        input_method::InputMethodManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            wlr_layer::{KeyboardInteractivity, WlrLayerShellState},
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
    },
};

#[cfg(feature = "xwayland")]
use smithay::{
    wayland::{
        xwayland_keyboard_grab::XWaylandKeyboardGrabState, xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::config::Config;
use crate::input::Binds;
use crate::ipc::IpcServer;
use crate::layout::{BarSpace, Fraction, Rect, ResizeEdges, work_area};
use crate::notify::{NotifyHandle, Snapshot, start_service};
use crate::runner::{Key as RunnerKey, Outcome as RunnerOutcome, Runner};
use crate::shell::{Layer, Shell, WindowId, tag_visible};
use crate::systray::{Snapshot as TraySnapshot, TrayHandle};
use crate::ui::{
    bar::{Bar, BarHit, BarView, NotifyView, RunnerCandidate, RunnerView, SystrayItem},
    cursor::PointerElement,
    icon::Icon,
    title::{TitleHit, TitlePlacement, TitleView, Titles},
    tray::TrayPopup,
};

use super::actions::spawn_shell;
use super::focus::KeyboardFocusTarget;

/// `CLOCK_MONOTONIC`，presentation-time 的时间基准。
const CLOCK_MONOTONIC: u32 = 1;

/// 服务端选区的来源标记。
///
/// 合成器自己当选区持有者只发生在一种情况：X11 程序拥有选区，需要把它
/// 以服务端选区的形式发布给 Wayland 客户端。Wayland 客户端读这个选区时，
/// `SelectionHandler::send_selection` 再去 X server 拉数据。
#[derive(Clone, Copy, Debug)]
pub struct X11Selection;

/// 合成器全局状态。
pub struct State {
    pub start_time: Instant,
    /// 本进程创建的 Wayland 套接字名；子进程通过 `WAYLAND_DISPLAY` 连上来。
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub config: Config,
    pub config_path: PathBuf,
    pub binds: Binds,

    /// 控制套接字；启动失败时为 `None`，合成器照常工作（ADR 0007）。
    pub ipc: Option<IpcServer>,

    /// 坐标系、层叠与命中测试的唯一真相。
    pub space: Space<Window>,
    /// 层、tag 与焦点顺序。
    pub shell: Shell,
    /// 当前获得键盘/动作焦点的输出；指针在哪个输出上就切到哪个。
    pub output: Option<Output>,
    /// 所有输出，按连接顺序；`output` 是它的一个成员。
    pub outputs: Vec<Output>,
    /// 每个输出自己的 bar；工作区由它预留。
    pub bars: HashMap<String, Bar>,
    /// 每个输出自己的标题条；工作区里的窗口由它扣出一条。
    pub title_bars: HashMap<String, Titles>,
    /// 每个输出扣掉 bar / layer 独占区后的工作区（逻辑坐标）。
    pub work_areas: HashMap<String, Rect>,
    /// bar 右侧的状态文本（基线从 stdin 读，见 `init_status_reader`）。
    pub status_text: String,
    /// `org.freedesktop.Notifications` 的控制句柄；没接管时为 `None`。
    pub notify: Option<NotifyHandle>,
    /// 通知服务推来的最新快照。
    pub notify_snapshot: Snapshot,
    /// 正在浏览的历史条目 id；`None` 表示看实时通知。
    pub notify_view: Option<u32>,
    /// 最近一次浏览输入的时刻（浏览会超时退出）。
    pub notify_view_at: u64,
    /// 当前显示的通知正文滚动了多少字节。
    pub notify_offset: usize,
    /// 通知动作选择器选中的动作序号。
    pub notify_pick: Option<usize>,
    /// 选择器属于哪条通知。
    pub notify_pick_id: Option<u32>,
    /// 当前正在显示的条目 id；变化时重置滚动偏移。
    notify_shown: Option<u32>,
    /// 通知区滚轮的累积量（基线一个刻度 15）。
    notify_wheel: f64,
    /// `org.kde.StatusNotifierWatcher` 的控制句柄；没接管时为 `None`。
    pub tray: Option<TrayHandle>,
    /// 托盘服务推来的最新快照。
    pub tray_snapshot: TraySnapshot,
    /// 托盘右键菜单的浮层。
    pub tray_popup: TrayPopup,
    /// 命令提示的完整状态（基线 `runner_active` 等一组全局变量）。
    pub runner: Runner,
    /// 窗口模式候选对应的窗口 id，与传给 runner 的标签一一对应。
    runner_windows: Vec<WindowId>,
    /// runner 候选图标的缓存（图标名 → 图标，`None` 表示查过但没有）。
    runner_icon_cache: HashMap<String, Option<Arc<Icon>>>,
    /// 状态行滚轮命令的累积量（基线一个鼠标刻度是 15）。
    status_scroll: f64,
    /// 扣掉 bar 与 layer-shell 独占区之后的工作区。
    pub work_area: Rect,
    /// 新浮动窗口的级联偏移步数。
    pub cascade: i32,

    /// 指针位置，供拖动 grab 与命中测试使用。
    pub pointer_location: Point<f64, Logical>,
    /// 最近一次键盘事件的修饰键状态（拖动判定要用）。
    pub keyboard_modifiers: ModifiersState,
    pub cursor_status: CursorImageStatus,
    /// 光标主题与渲染元素（命名光标 + 客户端自绘光标）。
    pub pointer_element: PointerElement,
    /// layer-shell 的独占键盘焦点；有值时窗口焦点不许抢键盘。
    pub layer_keyboard_focus: Option<WlSurface>,

    /// 画面是否有未画出来的变化。后端在事件循环空闲时据此决定要不要出帧，
    /// 没有变化就彻底不画——空闲桌面不该持续占用 GPU。
    pub needs_redraw: bool,

    /// winit 后端的渲染器与 damage 跟踪器；dmabuf 导入要直接拿到渲染器。
    #[cfg(feature = "winit")]
    pub winit_data: Option<crate::backend::winit::WinitData>,
    /// udev 后端的渲染器、DRM 输出与输入会话。
    #[cfg(feature = "udev")]
    pub udev_data: Option<crate::backend::udev::UdevData>,

    // Smithay 协议状态
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_activation_state: XdgActivationState,
    pub layer_shell_state: WlrLayerShellState,
    pub viewporter_state: ViewporterState,
    pub presentation_state: PresentationState,
    pub dmabuf_state: DmabufState,
    /// 注册成功的 dmabuf global；后端没有渲染器时为 `None`。
    pub dmabuf_global: Option<DmabufGlobal>,
    /// wp_fractional_scale_manager_v1；分数缩放客户端据此拿 preferred_scale。
    pub fractional_scale_state: FractionalScaleManagerState,

    /// xwayland-shell 协议；X11 窗口靠它把 X11 surface 和 wl_surface 关联起来。
    #[cfg(feature = "xwayland")]
    pub xwayland_shell_state: XWaylandShellState,
    /// X11 客户端申请键盘 grab 时用。
    #[cfg(feature = "xwayland")]
    pub xwayland_keyboard_grab_state: XWaylandKeyboardGrabState,
    /// 运行中的 X11 window manager；XWayland 就绪后才有值。
    #[cfg(feature = "xwayland")]
    pub xwm: Option<X11Wm>,
    /// XWayland 的 display number（`DISPLAY=:N`）。
    #[cfg(feature = "xwayland")]
    pub xdisplay: Option<u32>,
    /// 不受管理的 override-redirect 窗口（菜单、tooltip、DND 图标）。
    #[cfg(feature = "xwayland")]
    pub x11_override: Vec<Window>,
}

impl State {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
        config_path: PathBuf,
    ) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let presentation_state = PresentationState::new::<Self>(&display_handle, CLOCK_MONOTONIC);
        // dmabuf 的 global 要等后端渲染器就绪后才能注册，所以状态在这里先建。
        let dmabuf_state = DmabufState::new();
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&display_handle);
        #[cfg(feature = "xwayland")]
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
        #[cfg(feature = "xwayland")]
        let xwayland_keyboard_grab_state = XWaylandKeyboardGrabState::new::<Self>(&display_handle);
        // IME：text-input-v3 给客户端（fcitx5/ibus 通过它把预编辑文本送进来），
        // input-method-v2 给输入法进程。两者必须一起注册，Smithay 会在键盘焦点
        // 切换时自动同步 text-input 的 enter/leave。
        TextInputManagerState::new::<Self>(&display_handle);
        InputMethodManagerState::new::<Self, _>(&display_handle, |_client| true);
        // 输入法的兜底通道：不支持 text-input-v3 的客户端（部分终端/游戏）
        // 由 fcitx5 通过虚拟键盘注入按键。
        VirtualKeyboardManagerState::new::<Self, _>(&display_handle, |_client| true);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&display_handle, "r0wm");
        let keyboard = &config.input.keyboard;
        seat.add_keyboard(
            XkbConfig {
                rules: &keyboard.rules,
                model: &keyboard.model,
                layout: &keyboard.layout,
                variant: &keyboard.variant,
                options: (!keyboard.options.is_empty()).then(|| keyboard.options.clone()),
            },
            keyboard.repeat_delay,
            keyboard.repeat_rate,
        )
        .expect("键盘配置非法（检查 input.keyboard.xkb）");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();

        #[cfg(feature = "xwayland")]
        let xdisplay = super::xwayland::start(event_loop, &display_handle);

        let ipc = match IpcServer::start(event_loop, &socket_name.to_string_lossy()) {
            Ok(server) => {
                tracing::info!(path = %server.path().display(), "IPC 就绪");
                Some(server)
            }
            Err(err) => {
                tracing::warn!("IPC 启动失败，继续无 IPC 运行：{err:#}");
                None
            }
        };

        let binds = Binds::from_config(&config);
        let shell = Shell::new(
            config.layout.default.into(),
            Fraction::new(config.layout.default_width),
        );
        let runner = Runner::new(config.general.runner_config());

        // 通知服务跑在独立线程；快照通过 calloop channel 回到渲染线程。
        // 没有 bar 就没有显示位置，基线同样只在 `showbar` 时启动通知。
        let (notify_sender, notify_receiver) = calloop::channel::channel::<Snapshot>();
        let notify = if config.general.show_notifications && config.general.bar_enabled {
            start_service(notify_sender, config.general.notification_timeout)
        } else {
            None
        };
        if let Err(err) = event_loop
            .handle()
            .insert_source(notify_receiver, |event, _, state| {
                if let calloop::channel::Event::Msg(snapshot) = event {
                    state.apply_notify_snapshot(snapshot);
                }
            })
        {
            tracing::warn!("通知事件源注册失败：{err}");
        }

        // 托盘同样在独立线程里跑 D-Bus；没有 bar 就没有显示位置。
        let (tray_sender, tray_receiver) = calloop::channel::channel::<TraySnapshot>();
        let tray = if config.general.show_systray && config.general.bar_enabled {
            crate::systray::start(tray_sender, 48)
        } else {
            None
        };
        if let Err(err) = event_loop
            .handle()
            .insert_source(tray_receiver, |event, _, state| {
                if let calloop::channel::Event::Msg(snapshot) = event {
                    state.apply_tray_snapshot(snapshot);
                }
            })
        {
            tracing::warn!("托盘事件源注册失败：{err}");
        }
        let tray_popup = TrayPopup::new(&config.general);

        if let Some(command) = config.general.status_command.as_deref() {
            super::init_status_command(event_loop, command);
        }

        Self {
            start_time: Instant::now(),
            socket_name,
            display_handle,
            loop_signal,
            config,
            config_path,
            binds,
            ipc,
            space: Space::default(),
            shell,
            output: None,
            outputs: Vec::new(),
            bars: HashMap::new(),
            title_bars: HashMap::new(),
            work_areas: HashMap::new(),
            status_text: String::new(),
            notify,
            notify_snapshot: Snapshot::default(),
            notify_view: None,
            notify_view_at: 0,
            notify_offset: 0,
            notify_pick: None,
            notify_pick_id: None,
            notify_shown: None,
            notify_wheel: 0.0,
            tray,
            tray_snapshot: TraySnapshot::default(),
            tray_popup,
            runner,
            runner_windows: Vec::new(),
            runner_icon_cache: HashMap::new(),
            status_scroll: 0.0,
            work_area: Rect::default(),
            cascade: 0,
            pointer_location: Point::from((0.0, 0.0)),
            keyboard_modifiers: ModifiersState::default(),
            cursor_status: CursorImageStatus::default_named(),
            pointer_element: PointerElement::new(),
            layer_keyboard_focus: None,
            needs_redraw: true,
            #[cfg(feature = "winit")]
            winit_data: None,
            #[cfg(feature = "udev")]
            udev_data: None,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            popups: PopupManager::default(),
            seat,
            xdg_decoration_state,
            xdg_activation_state,
            layer_shell_state,
            viewporter_state,
            presentation_state,
            dmabuf_state,
            dmabuf_global: None,
            fractional_scale_state,
            #[cfg(feature = "xwayland")]
            xwayland_shell_state,
            #[cfg(feature = "xwayland")]
            xwayland_keyboard_grab_state,
            #[cfg(feature = "xwayland")]
            xwm: None,
            #[cfg(feature = "xwayland")]
            xdisplay,
            #[cfg(feature = "xwayland")]
            x11_override: Vec::new(),
        }
    }

    /// 监听客户端连接，并把 `Display` 挂进事件循环。
    fn init_wayland_listener(display: Display<Self>, event_loop: &mut EventLoop<Self>) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().expect("创建 Wayland 套接字失败");
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                if let Err(err) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!("接入客户端失败：{err}");
                }
            })
            .expect("注册套接字事件源失败");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // 安全：display 由本作用域持有，且在事件循环存活期间不会被 drop。
                    unsafe {
                        if let Err(err) = display.get_mut().dispatch_clients(state) {
                            tracing::error!("分发客户端事件失败：{err}");
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("注册 Display 事件源失败");

        socket_name
    }

    /// 注册输出、应用配置里的 scale/transform/position，并换算工作区。
    pub fn add_output(&mut self, output: Output) {
        let name = output.name();
        let config = self
            .config
            .outputs
            .iter()
            .find(|rule| rule.name == name)
            .cloned();
        if config.as_ref().is_some_and(|rule| !rule.enabled) {
            tracing::info!(name, "配置禁用了这个输出，跳过");
            return;
        }

        let scale = config
            .as_ref()
            .and_then(|rule| rule.scale)
            .map(smithay::output::Scale::Fractional);
        let transform = config
            .as_ref()
            .and_then(|rule| rule.transform.as_deref())
            .and_then(parse_transform);
        let position = config
            .as_ref()
            .and_then(|rule| rule.position)
            .unwrap_or_else(|| self.auto_output_position());
        output.change_current_state(None, transform, scale, Some(position.into()));

        let _global = output.create_global::<Self>(&self.display_handle);
        self.space.map_output(&output, position);
        self.outputs.push(output.clone());
        self.shell.add_output(&name);
        self.bars
            .insert(name.clone(), Bar::new(&self.config.general));
        self.title_bars
            .insert(name.clone(), Titles::new(&self.config.general));
        if self.output.is_none() {
            self.output = Some(output);
        }
        self.refresh_work_area();
    }

    /// 第二个及以后的输出默认排在前一个的右边。
    fn auto_output_position(&self) -> (i32, i32) {
        let mut right = 0;
        for other in &self.outputs {
            if let Some(geo) = self.space.output_geometry(other) {
                right = right.max(geo.loc.x + geo.size.w);
            }
        }
        let x = if self.outputs.is_empty() { 0 } else { right };
        (x, 0)
    }

    /// 输出下线：摘掉它的 UI 与布局状态，把窗口迁到剩下的输出。
    pub fn remove_output(&mut self, output: &Output) {
        let name = output.name();
        self.outputs.retain(|candidate| candidate != output);
        self.bars.remove(&name);
        self.title_bars.remove(&name);
        self.work_areas.remove(&name);
        self.shell.remove_output(&name);
        self.space.unmap_output(output);

        if let Some(next) = self.outputs.first().map(|output| output.name()) {
            let ids: Vec<WindowId> = self
                .shell
                .iter()
                .filter(|entry| entry.output == name)
                .map(|entry| entry.id)
                .collect();
            for id in ids {
                self.shell.move_to_output(id, &next);
            }
            if self.output.as_ref() == Some(output) {
                self.output = self.outputs.first().cloned();
            }
            self.refresh_work_area();
        } else {
            self.output = None;
            self.work_area = Rect::default();
        }
        self.space.refresh();
    }

    /// 热重载后把新的 output 规则作用到已连接输出（模式变更仍需重新插拔）。
    pub fn reapply_output_config(&mut self) {
        let outputs: Vec<Output> = self.outputs.clone();
        for output in outputs {
            let Some(rule) = self
                .config
                .outputs
                .iter()
                .find(|rule| rule.name == output.name())
                .cloned()
            else {
                continue;
            };
            let scale = rule.scale.map(smithay::output::Scale::Fractional);
            let transform = rule.transform.as_deref().and_then(parse_transform);
            let position = rule.position;
            output.change_current_state(None, transform, scale, position.map(Into::into));
            if let Some(position) = position {
                self.space.map_output(&output, position);
            }
        }
        self.refresh_work_area();
    }

    /// 输出尺寸/缩放/位置变化后重新计算工作区。
    pub fn refresh_work_area(&mut self) {
        let names: Vec<String> = self.outputs.iter().map(|output| output.name()).collect();
        let mut changed = false;
        for name in names {
            let Some(output) = self
                .outputs
                .iter()
                .find(|output| output.name() == name)
                .cloned()
            else {
                continue;
            };
            let Some(geo) = self.space.output_geometry(&output) else {
                continue;
            };
            let output_rect = Rect {
                x: geo.loc.x,
                y: geo.loc.y,
                width: geo.size.w,
                height: geo.size.h,
            };
            let bar = self
                .bars
                .get(&name)
                .map(|bar| BarSpace {
                    height: bar.height(),
                    at_top: bar.at_top(),
                })
                .unwrap_or_default();
            let mut work = work_area(output_rect, bar);
            {
                let mut map = smithay::desktop::layer_map_for_output(&output);
                map.arrange();
                let zone = map.non_exclusive_zone();
                if zone.size.w > 0 && zone.size.h > 0 {
                    let zone = Rect {
                        x: geo.loc.x + zone.loc.x,
                        y: geo.loc.y + zone.loc.y,
                        width: zone.size.w,
                        height: zone.size.h,
                    };
                    work = intersect(work, zone);
                }
            }
            if self.work_areas.get(&name) != Some(&work) {
                changed = true;
                self.work_areas.insert(name.clone(), work);
            }
            if self.output.as_ref().is_some_and(|out| out.name() == name) {
                self.work_area = work;
            }
        }

        if changed {
            self.arrange();
        }
        self.refresh_surface_scales();
        self.publish_output_event();
    }

    /// 除 `name` 以外所有输出的桌面矩形。
    ///
    /// 平铺窗口/标题条只应该出现在自己的输出上；Scroll 行滚到边界后目标矩形
    /// 可能压到隔壁输出，合成器必须自己裁掉，Smithay 的 `Space` 不做归属判断。
    fn other_output_areas(&self, name: &str) -> Vec<Rect> {
        self.outputs
            .iter()
            .filter(|output| output.name() != name)
            .filter_map(|output| self.space.output_geometry(output))
            .map(|geo| Rect {
                x: geo.loc.x,
                y: geo.loc.y,
                width: geo.size.w,
                height: geo.size.h,
            })
            .collect()
    }

    /// 指定输出的 bar。
    pub fn bar_for(&self, output: &Output) -> Option<&Bar> {
        self.bars.get(&output.name())
    }

    /// 指定输出的标题条。
    pub fn titles_for(&self, output: &Output) -> Option<&Titles> {
        self.title_bars.get(&output.name())
    }

    /// 当前焦点输出的 bar；没有输出时 `None`。
    pub fn focused_bar(&self) -> Option<&Bar> {
        self.output
            .as_ref()
            .and_then(|output| self.bars.get(&output.name()))
    }

    /// 当前焦点输出的 bar（可变）。
    pub fn focused_bar_mut(&mut self) -> Option<&mut Bar> {
        let name = self.output.as_ref().map(|output| output.name())?;
        self.bars.get_mut(&name)
    }

    /// 当前焦点输出的 bar 高度；通知/菜单排版用。
    pub fn bar_height(&self) -> i32 {
        self.focused_bar().map(Bar::height).unwrap_or(0)
    }

    /// 切换动作/键盘焦点到某个输出。
    pub fn focus_output(&mut self, output: &str) {
        if let Some(next) = self
            .outputs
            .iter()
            .find(|candidate| candidate.name() == output)
            .cloned()
        {
            self.work_area = self
                .work_areas
                .get(output)
                .copied()
                .unwrap_or(self.work_area);
            self.output = Some(next);
        }
    }

    /// 当前焦点输出的名字；没有输出时是空串。
    pub fn focused_output_name(&self) -> String {
        self.output
            .as_ref()
            .map(|output| output.name())
            .or_else(|| self.outputs.first().map(|output| output.name()))
            .unwrap_or_default()
    }

    /// 兼容单输出调用点：当前焦点输出的工作区。
    pub fn focused_work_area(&self) -> Rect {
        self.output
            .as_ref()
            .and_then(|output| self.work_areas.get(&output.name()).copied())
            .unwrap_or(self.work_area)
    }

    /// 把输出的当前分数缩放告诉某个 surface（若它建了 wp_fractional_scale_v1）。
    pub fn send_fractional_scale(&self, surface: &WlSurface) {
        let output = self
            .output_for_surface(surface)
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        with_states(surface, |states| {
            with_fractional_scale(states, |data| data.set_preferred_scale(scale));
        });
    }

    fn output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        let id = self.shell.id_of_surface(surface)?;
        let entry = self.shell.find(id)?;
        self.outputs
            .iter()
            .find(|output| output.name() == entry.output)
            .cloned()
    }

    /// 输出缩放变化后，把新的 preferred_scale 发给所有已映射的顶层与 layer。
    pub fn refresh_surface_scales(&self) {
        for window in self.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                self.send_fractional_scale(toplevel.wl_surface());
            }
        }
        for output in &self.outputs {
            let scale = output.current_scale().fractional_scale();
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                let surface = layer.wl_surface();
                with_states(surface, |states| {
                    with_fractional_scale(states, |data| data.set_preferred_scale(scale));
                });
            }
        }
    }

    /// 收集这一帧的呈现回执（窗口 + layer-shell 顶层），交给后端在 vblank 后回给客户端。
    pub fn collect_presentation_feedback(
        &self,
        output: &Output,
    ) -> smithay::desktop::utils::OutputPresentationFeedback {
        let mut feedback = smithay::desktop::utils::OutputPresentationFeedback::new(output);

        for window in self.space.elements() {
            if !self.space.outputs_for_element(window).contains(output) {
                continue;
            }
            window.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
            );
        }

        {
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                layer.take_presentation_feedback(
                    &mut feedback,
                    |_, _| Some(output.clone()),
                    |_, _| smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                );
            }
        }

        feedback
    }

    /// 用最新状态刷新 bar；内容没变时不做任何事。
    pub fn update_bar(&mut self, output: &Output) {
        let Some(geo) = self.space.output_geometry(output) else {
            return;
        };
        let name = output.name();
        let scale = output.current_scale().fractional_scale();
        let logical = geo.size;
        let visible = self.shell.visible_tags(&name);
        let occupied = self.shell.iter().fold(0u32, |acc, entry| {
            if entry.output == name {
                acc | entry.tags
            } else {
                acc
            }
        });
        let focused_tags = self
            .shell
            .focus()
            .and_then(|id| self.shell.find(id))
            .filter(|entry| entry.output == name)
            .map(|entry| entry.tags)
            .unwrap_or(0);
        let layout = self
            .shell
            .layout_of(&name, self.shell.focused_tag(&name))
            .name()
            .to_string();
        // 基线 `barwintitle` 默认关：焦点标题画在窗口自己的标题条上。
        let title = if self.config.general.bar_window_title {
            self.shell
                .focus()
                .and_then(|id| self.shell.find(id))
                .filter(|entry| entry.output == name)
                .map(|entry| {
                    entry
                        .title
                        .clone()
                        .or_else(|| entry.app_id.clone())
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        } else {
            String::new()
        };

        let notify = self.notify_bar_view();
        let runner = self.runner_view();

        let view = BarView {
            width: logical.w,
            scale,
            visible_tags: visible,
            occupied_tags: occupied,
            focused_tags,
            layout,
            title,
            status: self.status_text.clone(),
            notify,
            runner,
            systray: self
                .tray_snapshot
                .items
                .iter()
                .map(|item| SystrayItem {
                    appid: item.appid.clone(),
                    icon: item.icon.clone(),
                })
                .collect(),
        };
        if let Some(bar) = self.bars.get_mut(&name) {
            bar.update(view);
        }
    }

    // ---- 通知 ----

    /// 单调毫秒，与通知服务线程同一基准（进程启动）。
    fn now_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// 通知服务推来新快照：维护浏览/选择器的生命周期。
    pub fn apply_notify_snapshot(&mut self, snapshot: Snapshot) {
        let old_live = self.notify_snapshot.current.as_ref().map(|item| item.id);
        let new_live = snapshot.current.as_ref().map(|item| item.id);
        // 实时通知换了，历史浏览结束（基线 notifyshown）。
        if old_live != new_live {
            self.notify_view = None;
            self.close_notify_picker();
        }
        // 正在浏览的条目被删掉了。
        if let Some(id) = self.notify_view
            && !snapshot.history.iter().any(|item| item.id == id)
            && new_live != Some(id)
        {
            self.notify_view = None;
        }
        // 选择器所属的通知不在了，或者动作没了。
        if let Some(id) = self.notify_pick_id {
            let valid = snapshot
                .current
                .as_ref()
                .is_some_and(|item| item.id == id && !item.actions.is_empty());
            if !valid {
                self.close_notify_picker();
            } else if let Some(selected) = self.notify_pick
                && let Some(item) = snapshot.current.as_ref()
                && selected >= item.actions.len()
            {
                self.notify_pick = Some(item.actions.len().saturating_sub(1));
            }
        }
        self.notify_snapshot = snapshot;
        self.request_redraw();
    }

    /// 当前该显示的那条通知（实时或历史）及其历史位置。
    fn notify_item(&self) -> Option<(crate::notify::Item, Option<(usize, usize)>)> {
        if let Some(id) = self.notify_view {
            let history = &self.notify_snapshot.history;
            if let Some(index) = history.iter().position(|item| item.id == id) {
                return Some((history[index].clone(), Some((index + 1, history.len()))));
            }
        }
        self.notify_snapshot
            .current
            .clone()
            .map(|item| (item, None))
    }

    /// 组装 bar 中间区域的通知视图；没有可显示内容时为 `None`。
    fn notify_bar_view(&mut self) -> Option<NotifyView> {
        if !self.config.general.show_notifications {
            return None;
        }
        let now = self.now_ms();
        // 历史浏览会超时；基线用同一个 notification_timeout。
        if self.notify_view.is_some()
            && now >= self.notify_view_at + self.config.general.notification_timeout as u64 * 1000
        {
            self.notify_view = None;
        }
        let (item, history_pos) = self.notify_item()?;
        if self.notify_shown != Some(item.id) {
            self.notify_shown = Some(item.id);
            self.notify_offset = 0;
        }
        let pick = self
            .notify_pick
            .filter(|_| self.notify_pick_id == Some(item.id));
        Some(NotifyView {
            id: item.id,
            text: item.text,
            urgency: item.urgency,
            history: history_pos.is_some(),
            history_pos,
            queued: self.notify_snapshot.queued,
            left_ms: item.left_ms,
            timeout_ms: item.timeout_ms,
            actions: item.actions,
            pick,
            value: item.value,
            icon: item.icon,
            offset: self.notify_offset,
        })
    }

    // ---- 托盘 ----

    /// 托盘服务推来新快照：更新图标列表，按需弹出/更新/收起菜单。
    pub fn apply_tray_snapshot(&mut self, snapshot: TraySnapshot) {
        self.tray_snapshot = snapshot;
        match self.tray_snapshot.menu.clone() {
            Some(menu) if !menu.labels.is_empty() => {
                // 层没变就不要重置位置和悬停。
                if self.tray_popup.labels() != menu.labels.as_slice() {
                    let output = self.output.clone();
                    let geometry = output.as_ref().and_then(|o| self.space.output_geometry(o));
                    let scale = output
                        .as_ref()
                        .map(|o| o.current_scale().fractional_scale())
                        .unwrap_or(1.0);
                    if let Some(geometry) = geometry {
                        self.tray_popup.show(
                            menu.labels,
                            self.pointer_location,
                            geometry,
                            self.bar_height(),
                            scale,
                            0,
                        );
                    }
                }
            }
            _ => self.tray_popup.hide(),
        }
        self.refresh_bar();
    }

    /// 左键点击第 `index` 个托盘图标：调用客户端的 `Activate`。
    pub fn tray_activate(&self, index: usize) {
        if let (Some(handle), Some(item)) = (&self.tray, self.tray_snapshot.items.get(index)) {
            handle.activate(&item.busname);
        }
    }

    /// 右键点击第 `index` 个托盘图标：拉取它的 dbusmenu。
    pub fn tray_menu(&self, index: usize) {
        if let (Some(handle), Some(item)) = (&self.tray, self.tray_snapshot.items.get(index)) {
            handle.open_menu(&item.busname);
        }
    }

    /// 托盘菜单正在显示时截图指针事件。
    pub fn tray_popup_active(&self) -> bool {
        self.tray_popup.active()
    }

    /// 菜单里悬停行变化；返回是否重画。
    pub fn tray_popup_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        self.tray_popup.motion(pos)
    }

    /// 点击菜单：行内选中，行外取消（基线 `traypopup_click`）。
    pub fn tray_popup_click(&mut self, pos: Point<f64, Logical>) {
        if !self.tray_popup.active() {
            return;
        }
        let row = self.tray_popup.row_at(pos);
        self.tray_popup.hide();
        if let Some(handle) = &self.tray {
            handle.pick(row.map(|row| row as i32).unwrap_or(-1));
        }
    }

    /// 收起菜单（Escape 或点外）。
    pub fn tray_popup_dismiss(&mut self) {
        if !self.tray_popup.active() {
            return;
        }
        self.tray_popup.hide();
        if let Some(handle) = &self.tray {
            handle.pick(-1);
        }
    }

    // ---- 命令提示（runner） ----

    /// 打开/收起提示。打开前按焦点栈与 tag 刷新候选来源。
    pub fn runner_toggle(&mut self) {
        if self.runner.active() {
            self.runner.close();
        } else {
            self.runner_sync();
            self.runner.open();
        }
        self.refresh_bar();
    }

    /// 把焦点栈（最近使用在前）与 tag 名推给 runner。
    fn runner_sync(&mut self) {
        let mut windows = Vec::new();
        self.runner_windows.clear();
        for id in self.shell.focus_order() {
            let Some(entry) = self.shell.find(id) else {
                continue;
            };
            let app = entry.app_id.clone().unwrap_or_default();
            let title = entry.title.clone().unwrap_or_default();
            let label = if title.is_empty() || title == app {
                app
            } else if app.is_empty() {
                title
            } else {
                format!("{app}: {title}")
            };
            if label.is_empty() {
                continue;
            }
            self.runner_windows.push(id);
            windows.push(label);
        }
        self.runner.set_windows(windows);
        self.runner
            .set_tags((1..=9).map(|tag| tag.to_string()).collect());
    }

    /// 提示独占键盘时把按键交给它；返回是否消费了本次按键。
    pub fn runner_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        use smithay::input::keyboard::keysyms;

        let key = match sym.raw() {
            keysyms::KEY_Escape => RunnerKey::Escape,
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => RunnerKey::Enter,
            keysyms::KEY_Tab => RunnerKey::Tab,
            keysyms::KEY_ISO_Left_Tab => RunnerKey::BackTab,
            keysyms::KEY_BackSpace => RunnerKey::Backspace,
            keysyms::KEY_Delete | keysyms::KEY_KP_Delete => RunnerKey::Delete,
            keysyms::KEY_Left | keysyms::KEY_KP_Left => RunnerKey::Left,
            keysyms::KEY_Right | keysyms::KEY_KP_Right => RunnerKey::Right,
            keysyms::KEY_Up | keysyms::KEY_KP_Up => RunnerKey::Up,
            keysyms::KEY_Down | keysyms::KEY_KP_Down => RunnerKey::Down,
            keysyms::KEY_Home | keysyms::KEY_KP_Home => RunnerKey::Home,
            keysyms::KEY_End | keysyms::KEY_KP_End => RunnerKey::End,
            _ => match sym.key_char() {
                Some(ch) => RunnerKey::Char(ch),
                None => return false,
            },
        };
        let outcome = self.runner.key(key, mods.ctrl, mods.shift);
        self.runner_apply(outcome);
        self.refresh_bar();
        true
    }

    /// 执行提示给出的结局（基线 `runnerenter` 的 switch）。
    fn runner_apply(&mut self, outcome: RunnerOutcome) {
        match outcome {
            RunnerOutcome::Continue => {}
            RunnerOutcome::Close => self.runner.close(),
            RunnerOutcome::Exec { cmd, term } => {
                let general = &self.config.general;
                let terminal = term.then_some(general.runner_terminal.as_str());
                // 只有配了额外目录时才给子进程换 PATH（基线同样只在此时改）。
                let path = if general.runner_paths.is_empty() {
                    None
                } else {
                    Some(
                        crate::runner::apps::search_paths(&general.runner_paths)
                            .iter()
                            .map(|dir| dir.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(":"),
                    )
                };
                super::actions::spawn_line(&general.runner_shell, &cmd, terminal, path.as_deref());
            }
            RunnerOutcome::FocusWindow(index) => {
                self.runner_focus(index);
            }
            RunnerOutcome::ViewTag(index) => {
                let mask = 1u32 << index.min(31);
                self.execute(crate::action::Action::View(mask));
            }
            RunnerOutcome::Open(target) => super::actions::spawn_open(&target),
            RunnerOutcome::Search(query) => {
                let base = self.config.general.runner_search_url.clone();
                super::actions::spawn_open(&crate::runner::search_url(&base, &query));
            }
        }
    }

    /// 切到候选窗口所在的 tag 并聚焦（基线 `activateclient`）。
    fn runner_focus(&mut self, index: usize) {
        let Some(id) = self.runner_windows.get(index).copied() else {
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
        self.after_focus_change();
    }

    /// 输出就绪时立刻重画 bar（没有输出时下一帧会自然刷新）。
    fn refresh_bar(&mut self) {
        if let Some(output) = self.output.clone() {
            self.update_bar(&output);
        }
        self.request_redraw();
    }

    /// 提示打开时的呈现快照。
    fn runner_view(&mut self) -> Option<RunnerView> {
        if !self.runner.active() {
            return None;
        }
        let config = self.runner.config();
        let per = config.max_candidates.max(1);
        let placeholder = config.placeholder.clone();
        let mode = self.config.general.runner_icons;
        let selected = self.runner.selected();
        let start = self.runner.page_start();
        let total = self.runner.candidates().len();
        let has_icons = self.runner.has_icons();
        // 先把名字抄出来，再按需要读图标缓存：runner 的借用不能和 `&mut self` 重叠。
        let page: Vec<(String, Option<String>)> = (start..total.min(start + per))
            .filter_map(|index| {
                let label = self.runner.candidate(index)?.to_owned();
                let icon = if mode != crate::config::RunnerIcons::None
                    && (mode == crate::config::RunnerIcons::All || index == selected)
                {
                    self.runner.icon(index).map(str::to_owned)
                } else {
                    None
                };
                Some((label, icon))
            })
            .collect();
        let candidates: Vec<RunnerCandidate> = page
            .into_iter()
            .map(|(label, icon)| RunnerCandidate {
                label,
                icon: icon.and_then(|name| self.runner_icon(&name)),
            })
            .collect();
        let query = self.runner.query().to_owned();
        let completion = self
            .runner
            .suggest()
            .and_then(|suggest| suggest.strip_prefix(query.as_str()))
            .filter(|tail| !tail.is_empty())
            .map(str::to_owned);
        let calc = if completion.is_none() {
            self.runner.calc_text()
        } else {
            None
        };
        Some(RunnerView {
            text: self.runner.buf().to_string(),
            cursor: self.runner.cursor(),
            placeholder,
            completion,
            calc,
            candidates,
            selected: selected.saturating_sub(start),
            has_icons,
        })
    }

    /// 按名字取 runner 候选图标；`None` 也缓存，避免反复去主题目录里找。
    fn runner_icon(&mut self, name: &str) -> Option<Arc<Icon>> {
        if let Some(icon) = self.runner_icon_cache.get(name) {
            return icon.clone();
        }
        let icon =
            crate::ui::icon::load_named(name, None, crate::notify::ICON_SIZE as u32).map(Arc::new);
        self.runner_icon_cache.insert(name.to_owned(), icon.clone());
        icon
    }

    /// 打开通知：先尽量聚焦发送者的窗口，再触发 `default` 动作。
    pub fn notify_open(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        self.close_notify_picker();
        let Some((item, history)) = self.notify_item() else {
            return;
        };
        self.focus_notify_app(&item);
        if history.is_some() {
            self.notify_view = None;
            return;
        }
        let token = self.activation_token();
        if let Some(handle) = self.notify.as_ref() {
            handle.invoke(token);
        }
    }

    /// 从 bar 上点通知正文：选择器开着时先关掉它（基线 `notifyopen`）。
    pub fn notify_open_click(&mut self) {
        if self.notify_pick.is_some() {
            self.close_notify_picker();
            return;
        }
        self.notify_open();
    }

    /// 关掉当前通知；浏览历史时删掉这一条并挪到下一条。
    pub fn notify_dismiss(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        if self.notify_pick.is_some() {
            self.close_notify_picker();
            return;
        }
        let Some((item, history)) = self.notify_item() else {
            return;
        };
        let Some((pos, len)) = history else {
            if let Some(handle) = self.notify.as_ref() {
                handle.dismiss();
            }
            return;
        };
        if let Some(handle) = self.notify.as_ref() {
            handle.history_remove(item.id);
        }
        // 删掉第 pos 条之后：优先看原来它后面的那条，否则看前面一条。
        let history = &self.notify_snapshot.history;
        self.notify_view = if pos < len {
            history.get(pos).map(|item| item.id)
        } else {
            history.get(pos.saturating_sub(2)).map(|item| item.id)
        };
        self.notify_view_at = self.now_ms();
    }

    /// 当前与排队的通知全部关掉。
    pub fn notify_dismiss_all(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        self.close_notify_picker();
        self.notify_view = None;
        if let Some(handle) = self.notify.as_ref() {
            handle.dismiss_all();
        }
    }

    /// 历史里向新的一条走；在实时通知上则关掉它。
    pub fn notify_next(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        self.close_notify_picker();
        let Some((_, history)) = self.notify_item() else {
            return;
        };
        match history {
            Some((pos, _)) if pos > 1 => {
                self.notify_view = self
                    .notify_snapshot
                    .history
                    .get(pos - 2)
                    .map(|item| item.id);
                self.notify_view_at = self.now_ms();
            }
            Some(_) => self.notify_view = None,
            None => {
                if let Some(handle) = self.notify.as_ref() {
                    handle.dismiss();
                }
            }
        }
    }

    /// 历史里向旧的一条走；已经在最旧的就只刷新浏览超时。
    pub fn notify_prev(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        self.close_notify_picker();
        let Some((_, history)) = self.notify_item() else {
            return;
        };
        if let Some((pos, len)) = history {
            if pos < len {
                self.notify_view = self.notify_snapshot.history.get(pos).map(|item| item.id);
            }
            self.notify_view_at = self.now_ms();
        } else if let Some(item) = self.notify_snapshot.history.first() {
            // 实时通知上「向旧走」就是打开最新的一条历史。
            self.notify_view = Some(item.id);
            self.notify_view_at = self.now_ms();
        }
    }

    /// 正文翻到下一屏（基线 `notifyscroll`）。
    pub fn notify_scroll(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        let Some((item, history)) = self.notify_item() else {
            return;
        };
        if history.is_some() {
            self.notify_view_at = self.now_ms();
        }
        let offset = self.notify_offset;
        if let Some(bar) = self.focused_bar_mut() {
            let next = bar.notify_next_page(&item.text, offset);
            self.notify_offset = next;
        }
    }

    /// 展开/收起动作选择器；打开时暂停计时。
    pub fn notify_actions(&mut self) {
        if !self.config.general.show_notifications {
            return;
        }
        if self.notify_pick.is_some() {
            self.close_notify_picker();
            return;
        }
        let Some((item, history)) = self.notify_item() else {
            return;
        };
        if history.is_some() || item.actions.is_empty() {
            return;
        }
        self.notify_view = None;
        self.notify_pick = Some(0);
        self.notify_pick_id = Some(item.id);
        if let Some(handle) = self.notify.as_ref() {
            handle.hold(true);
        }
    }

    fn close_notify_picker(&mut self) {
        if self.notify_pick.take().is_some() {
            self.notify_pick_id = None;
            if let Some(handle) = self.notify.as_ref() {
                handle.hold(false);
            }
        }
    }

    /// 选择器键盘：左右/Tab 移动，回车/空格执行，Esc 关闭，1-9 直选。
    pub fn notify_pick_key(&mut self, syms: &[smithay::input::keyboard::Keysym]) -> bool {
        use smithay::input::keyboard::keysyms;
        let Some(selected) = self.notify_pick else {
            return false;
        };
        let Some(id) = self.notify_pick_id else {
            return false;
        };
        let Some(item) = self
            .notify_snapshot
            .current
            .as_ref()
            .filter(|item| item.id == id)
        else {
            self.close_notify_picker();
            return true;
        };
        let count = item.actions.len().max(1);
        for sym in syms {
            match sym.raw() {
                keysyms::KEY_Escape => {
                    self.close_notify_picker();
                    return true;
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                    self.notify_pick_run(selected);
                    return true;
                }
                keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_ISO_Left_Tab => {
                    self.notify_pick = Some((selected + count - 1) % count);
                    return true;
                }
                keysyms::KEY_Right | keysyms::KEY_l | keysyms::KEY_Tab => {
                    self.notify_pick = Some((selected + 1) % count);
                    return true;
                }
                raw => {
                    let digit = keysyms::KEY_1..=keysyms::KEY_9;
                    if digit.contains(&raw) {
                        let index = (raw - keysyms::KEY_1) as usize;
                        if index < count {
                            self.notify_pick_run(index);
                        }
                        return true;
                    }
                }
            }
        }
        // 选择器开着时吞掉其余按键，避免落到客户端。
        true
    }

    /// 执行选择器里的第 `index` 个动作。
    pub fn notify_pick_run(&mut self, index: usize) {
        self.close_notify_picker();
        let token = self.activation_token();
        if let Some(handle) = self.notify.as_ref() {
            handle.action(index, token);
        }
    }

    /// 通知区的滚轮：纵向走历史，横向滚正文。
    pub fn notify_wheel(&mut self, vertical: f64, horizontal: f64) -> bool {
        if !self.config.general.show_notifications {
            return false;
        }
        if self.notify_item().is_none() {
            return false;
        }
        if horizontal != 0.0 {
            if let Some((item, history)) = self.notify_item() {
                if history.is_some() {
                    self.notify_view_at = self.now_ms();
                }
                let step = if horizontal < 0.0 { -1 } else { 1 };
                if let Some(bar) = self.focused_bar() {
                    self.notify_offset =
                        bar.notify_scroll_chars(&item.text, self.notify_offset, step);
                }
            }
            return true;
        }
        const NOTCH: f64 = 15.0;
        if (self.notify_wheel < 0.0) != (vertical < 0.0) {
            self.notify_wheel = 0.0;
        }
        self.notify_wheel += vertical;
        while self.notify_wheel >= NOTCH {
            self.notify_wheel -= NOTCH;
            self.notify_next();
        }
        while self.notify_wheel <= -NOTCH {
            self.notify_wheel += NOTCH;
            self.notify_prev();
        }
        true
    }

    /// 焦点跳到通知的发送者（app_id 或 desktop-entry 匹配，忽略大小写）。
    fn focus_notify_app(&mut self, item: &crate::notify::Item) {
        let target = self
            .shell
            .iter()
            .find(|entry| {
                let app_id = entry.app_id.as_deref().unwrap_or("");
                (!app_id.is_empty() && app_id.eq_ignore_ascii_case(&item.app))
                    || (!item.desktop.is_empty() && app_id.eq_ignore_ascii_case(&item.desktop))
            })
            .map(|entry| (entry.id, entry.tags, entry.output.clone()));
        let Some((id, tags, output)) = target else {
            return;
        };
        self.focus_output(&output);
        if tags & self.shell.visible_tags(&output) == 0 {
            self.shell.set_visible_tags(&output, tags);
        }
        self.shell.set_focus(id);
        self.after_focus_change();
    }

    /// 给客户端一个激活令牌，让它在动作回调里能抬起自己的窗口。
    fn activation_token(&mut self) -> Option<String> {
        let (token, _) = self.xdg_activation_state.create_external_token(None);
        Some(token.as_str().to_string())
    }

    /// 用当前窗口状态刷新标题条；只在内容变化时重画位图。
    pub fn update_titles(&mut self, output: &Output) {
        let name = output.name();
        if !self
            .title_bars
            .get(&name)
            .is_some_and(|titles| titles.is_enabled())
        {
            return;
        }
        // 浮动窗口拖动/缩放时几何每帧都在变，位置在这一帧重新算一遍。
        self.sync_title_placements();
        let scale = output.current_scale().fractional_scale();
        let focused = self.shell.focus();
        let (hovered, placements) = match self.title_bars.get(&name) {
            Some(titles) => (titles.hovered(), titles.placements().to_vec()),
            None => return,
        };
        let mut views = Vec::with_capacity(placements.len());
        for slot in placements {
            let Some(entry) = self.shell.find(slot.id) else {
                continue;
            };
            let title = entry
                .title
                .clone()
                .or_else(|| entry.app_id.clone())
                .unwrap_or_default();
            views.push((
                slot.id,
                TitleView {
                    width: slot.rect.size.w,
                    scale,
                    title,
                    selected: focused == Some(slot.id),
                    floating: entry.layer == Layer::Floating,
                    tab: slot.tab,
                    first_tab: slot.first_tab,
                    hover: hovered == Some(slot.id),
                },
            ));
        }
        if let Some(titles) = self.title_bars.get_mut(&name) {
            for (id, view) in views {
                titles.update(id, view);
            }
        }
    }

    /// 指针下的标题条命中（关闭按钮单独标记）。
    pub fn title_hit(&self, pos: Point<f64, Logical>) -> Option<TitleHit> {
        self.title_bars.values().find_map(|titles| titles.hit(pos))
    }

    /// 计算某个输出的标题条位置：平铺窗口来自布局切片，浮动窗口自己一条。
    fn place_titles(&mut self, output: &str, area: Rect, th: i32, visible: u32) {
        if th <= 0 {
            if let Some(titles) = self.title_bars.get_mut(output) {
                titles.set_placements(Vec::new());
            }
            return;
        }
        let other_outputs = self.other_output_areas(output);
        let mut slots: Vec<TitlePlacement> = self
            .shell
            .title_slices(output, area)
            .into_iter()
            // 与窗口映射保持一致：滚出本输出的列连标题条也不要画到别的输出上。
            .filter(|slice| {
                slice.rect.overlaps(area)
                    && !other_outputs
                        .iter()
                        .any(|other| slice.rect.overlaps(*other))
            })
            .map(|slice| TitlePlacement {
                id: slice.id,
                rect: Rectangle::new(
                    Point::from((slice.rect.x, slice.rect.y)),
                    Size::from((slice.rect.width, th)),
                ),
                tab: slice.tab,
                first_tab: slice.first_tab,
            })
            .collect();
        for entry in self.shell.iter() {
            if entry.output != output
                || entry.layer != Layer::Floating
                || !tag_visible(entry.tags, entry.sticky, visible)
            {
                continue;
            }
            let Some((x, y)) = entry.floating_loc else {
                continue;
            };
            let width = entry.window.geometry().size.w.max(1);
            slots.push(TitlePlacement {
                id: entry.id,
                rect: Rectangle::new(Point::from((x, y)), Size::from((width, th))),
                tab: false,
                first_tab: true,
            });
        }
        if let Some(titles) = self.title_bars.get_mut(output) {
            titles.set_placements(slots);
        }
    }

    /// 标题条上方当前要给窗口内容留的高度（禁用时为 0）。
    pub fn title_height(&self) -> i32 {
        self.title_bars
            .values()
            .next()
            .map(|titles| titles.height())
            .unwrap_or(0)
    }

    /// 悬停状态变了就重画那两条标题条（下一帧 `update_titles` 会处理）。
    pub fn set_title_hover(&mut self, pos: Point<f64, Logical>) {
        for titles in self.title_bars.values_mut() {
            titles.set_hover(Some(pos));
        }
    }

    /// 拖动/缩放浮动窗口时标题条要跟着走；只重算位置，不触发整轮重排。
    pub fn sync_title_placements(&mut self) {
        let th = self.title_height();
        let names: Vec<String> = self.outputs.iter().map(|output| output.name()).collect();
        for name in names {
            let visible = self.shell.visible_tags(&name);
            let area = self
                .work_areas
                .get(&name)
                .copied()
                .unwrap_or(self.work_area);
            self.place_titles(&name, area, th, visible);
        }
    }

    /// 请求客户端关闭窗口（基线 `client_send_close`）。
    pub fn close_window(&mut self, id: WindowId) {
        if let Some(window) = self.shell.window(id) {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_close();
            }
            #[cfg(feature = "xwayland")]
            if let Some(surface) = window.x11_surface() {
                if let Err(err) = surface.close() {
                    tracing::warn!("关闭 X11 窗口失败：{err}");
                }
            }
        }
    }

    /// 全局坐标是否落在某个输出的 bar 上；返回 bar 内的命中段。
    pub fn bar_hit(&self, pos: Point<f64, Logical>) -> Option<BarHit> {
        let (name, local) = self.bar_local(pos)?;
        self.bars.get(&name).and_then(|bar| bar.hit(local))
    }

    /// 全局坐标是否落在 bar 条带内（不论具体是哪一段）。
    pub fn bar_contains(&self, pos: Point<f64, Logical>) -> bool {
        self.bar_local(pos).is_some()
    }

    /// 某个输出当前是否有全屏窗口；有则 bar 隐藏、工作区按整屏算。
    pub fn fullscreen_active(&self, output: &str) -> bool {
        let visible = self.shell.visible_tags(output);
        self.shell.iter().any(|entry| {
            entry.output == output
                && entry.layer == Layer::Fullscreen
                && tag_visible(entry.tags, entry.sticky, visible)
        })
    }

    /// bar 是否应该显示/接收输入：全屏窗口覆盖本输出时隐藏。
    pub fn bar_visible(&self, output: &str) -> bool {
        !self.fullscreen_active(output)
    }

    /// 把全局坐标换算成「输出名 + bar 内逻辑坐标」；不在任何 bar 上就是 `None`。
    fn bar_local(&self, pos: Point<f64, Logical>) -> Option<(String, Point<f64, Logical>)> {
        for output in &self.outputs {
            let name = output.name();
            if !self.bar_visible(&name) {
                continue;
            }
            let Some(bar) = self.bars.get(&name) else {
                continue;
            };
            if !bar.enabled() || bar.height() <= 0 {
                continue;
            }
            let Some(geo) = self.space.output_geometry(output) else {
                continue;
            };
            let bar_y = if bar.at_top() {
                geo.loc.y
            } else {
                geo.loc.y + geo.size.h - bar.height()
            };
            let band = Rectangle::new(
                Point::from((geo.loc.x, bar_y)),
                smithay::utils::Size::from((geo.size.w, bar.height())),
            );
            if band.to_f64().contains(pos) {
                return Some((name, pos - band.loc.to_f64()));
            }
        }
        None
    }

    /// 状态文本的点击命令：命中可点击片段就交给 `/bin/sh -c` 执行。
    pub fn status_click(&mut self, pos: Point<f64, Logical>, button: u8) -> bool {
        let Some((name, local)) = self.bar_local(pos) else {
            return false;
        };
        let Some(command) = self
            .bars
            .get(&name)
            .and_then(|bar| bar.status_command(local, button))
            .map(str::to_owned)
        else {
            return false;
        };
        spawn_shell(&command);
        true
    }

    /// 状态文本的滚轮命令：按基线的一个「刻度」15 累积，攒够一次执行一次。
    pub fn status_scroll(&mut self, pos: Point<f64, Logical>, delta: f64) -> bool {
        if delta == 0.0 {
            return false;
        }
        let Some((name, local)) = self.bar_local(pos) else {
            return false;
        };
        // 基线：滚轮上 = 按钮 4，滚轮下 = 按钮 5。
        let button = if delta < 0.0 { 4 } else { 5 };
        let Some(command) = self
            .bars
            .get(&name)
            .and_then(|bar| bar.status_command(local, button))
            .map(str::to_owned)
        else {
            return false;
        };
        // 换方向时丢掉半格，免得来回滚一次就触发。
        if (self.status_scroll < 0.0) != (delta < 0.0) {
            self.status_scroll = 0.0;
        }
        self.status_scroll += delta;
        const NOTCH: f64 = 15.0;
        let mut fired = false;
        while self.status_scroll >= NOTCH {
            self.status_scroll -= NOTCH;
            spawn_shell(&command);
            fired = true;
        }
        while self.status_scroll <= -NOTCH {
            self.status_scroll += NOTCH;
            spawn_shell(&command);
            fired = true;
        }
        fired
    }

    /// 点击是否落在某个根 surface（窗口或 layer）的 popup 范围内。
    /// 客户端不请求显式 grab 的 popup（工具提示、部分菜单）也要「点外关闭」，
    /// 这是 wlroots 的隐式 grab 行为。返回 true 表示这次按下点在某个 popup 里。
    pub fn popup_contains(&self, pos: Point<f64, Logical>) -> bool {
        let roots = self.popup_roots();
        for (root, origin) in roots {
            let point = pos - origin.to_f64();
            for (popup, relative) in smithay::desktop::PopupManager::popups_for_surface(&root) {
                let geometry = popup.geometry();
                if geometry.size.w <= 0 || geometry.size.h <= 0 {
                    continue;
                }
                let rect = Rectangle::new(relative, geometry.size);
                if rect.to_f64().contains(point) {
                    return true;
                }
            }
        }
        false
    }

    /// 关掉所有 popup，返回是否真的关掉了东西。
    ///
    /// 只关顶层 popup 即可：`dismiss_popup` 会沿树把子 popup 一起 done。
    pub fn dismiss_all_popups(&mut self) -> bool {
        let mut dismissed = false;
        for (root, _) in self.popup_roots() {
            let popups: Vec<_> = smithay::desktop::PopupManager::popups_for_surface(&root)
                .map(|(popup, _)| popup)
                .collect();
            for popup in popups {
                let _ = smithay::desktop::PopupManager::dismiss_popup(&root, &popup);
                dismissed = true;
            }
        }
        dismissed
    }

    /// 所有可能有 popup 的根 surface 及其在全局坐标里的原点。
    fn popup_roots(&self) -> Vec<(WlSurface, Point<i32, Logical>)> {
        let mut roots = Vec::new();
        for window in self.space.elements() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            roots.push((toplevel.wl_surface().clone(), location));
        }
        for output in &self.outputs {
            // 先克隆成局部变量再借 layer map，避免 `self` 的借用跨过迭代。
            let Some(output_geo) = self.space.output_geometry(output) else {
                continue;
            };
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                let Some(geometry) = map.layer_geometry(layer) else {
                    continue;
                };
                roots.push((layer.wl_surface().clone(), output_geo.loc + geometry.loc));
            }
        }
        roots
    }

    /// 指针位置对应的输出；没有命中时退回当前焦点输出。
    fn output_at(&self, pos: Point<f64, Logical>) -> Option<Output> {
        self.space
            .output_under(pos)
            .next()
            .cloned()
            .or_else(|| self.output.clone())
    }

    /// 指针下最深的 wayland surface，以及它在全局坐标里的位置。
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        use smithay::desktop::WindowSurfaceType;
        use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

        // layer-shell 在窗口之上（bar、锁屏、通知）；先按层从上到下命中。
        if let Some(output) = self.output_at(pos)
            && let Some(output_geo) = self.space.output_geometry(&output)
        {
            let origin = output_geo.loc.to_f64();
            let map = smithay::desktop::layer_map_for_output(&output);
            for layer in [
                WlrLayer::Overlay,
                WlrLayer::Top,
                WlrLayer::Bottom,
                WlrLayer::Background,
            ] {
                let Some(surface) = map.layer_under(layer, pos - origin) else {
                    continue;
                };
                let location = map
                    .layer_geometry(surface)
                    .map(|geometry| geometry.loc.to_f64())
                    .unwrap_or_default();
                if let Some((wl_surface, point)) =
                    surface.surface_under(pos - origin - location, WindowSurfaceType::ALL)
                {
                    return Some((wl_surface, point.to_f64() + location + origin));
                }
            }
        }

        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, (point + location).to_f64()))
            })
    }

    /// 指针是否落在任何 layer-shell 表面上（bar 等），此时不抢窗口焦点。
    pub fn pointer_over_layer(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_at(pos) else {
            return false;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return false;
        };
        let origin = output_geo.loc.to_f64();
        let map = smithay::desktop::layer_map_for_output(&output);
        map.layers().rev().any(|layer| {
            let location = map
                .layer_geometry(layer)
                .map(|geometry| geometry.loc.to_f64())
                .unwrap_or_default();
            layer
                .bbox_with_popups()
                .to_f64()
                .contains(pos - origin - location)
        })
    }

    /// 把窗口放到浮动层的初始位置：工作区居中 + 级联偏移。
    pub fn place_floating(&mut self, id: WindowId) {
        let Some(window) = self.shell.window(id).cloned() else {
            return;
        };
        let size = window.geometry().size;
        if size.w <= 0 || size.h <= 0 {
            return;
        }

        let output = self
            .shell
            .find(id)
            .map(|entry| entry.output.clone())
            .unwrap_or_default();
        let area = self
            .work_areas
            .get(&output)
            .copied()
            .unwrap_or_else(|| self.focused_work_area());
        let width = size.w.min(area.width);
        let height = size.h.min(area.height);
        let th = self.title_height();

        // 级联：每新开一个窗口右移 32 px，走完 5 步回到原点。
        let step = self.cascade % 5;
        self.cascade = self.cascade.wrapping_add(1);
        let shift = step * 32;

        let x = area.x + (area.width - width) / 2 + shift;
        // `floating_loc` 存标题条左上角，内容在它下面一条。
        let y = area.y + (area.height - (height + th)).max(0) / 2 + shift;

        self.space.map_element(window.clone(), (x, y + th), false);
        #[cfg(feature = "xwayland")]
        self.sync_x11_geometry(
            &window,
            Rectangle::new((x, y + th).into(), Size::from((width, height))),
        );
        if let Some(entry) = self.shell.find_mut(id) {
            entry.placed = true;
            entry.floating_loc = Some((x, y));
        }
        tracing::info!(id, x, y, width, height, "浮动窗口就位");
    }

    /// 新窗口的默认层、tag 与输出：平铺到当前输出的当前行。
    pub fn default_placement(&self) -> (Layer, u32, String) {
        let output = self
            .output
            .as_ref()
            .map(|output| output.name())
            .or_else(|| self.outputs.first().map(|output| output.name()))
            .unwrap_or_default();
        (Layer::Tiled, self.shell.focused_tag(&output), output)
    }

    /// 把窗口转为浮动层并保留当前屏幕位置，返回左上角。
    ///
    /// 从 tiled 转 floating 时 `floating_loc` 还是空的，必须先记下原位置，
    /// 否则紧接着的 `arrange` 会把窗口丢到工作区原点。
    pub fn float_window(&mut self, id: WindowId) -> Option<Point<i32, Logical>> {
        let window = self.shell.window(id)?.clone();
        let layer = self.shell.find(id)?.layer;
        let location = self
            .space
            .element_location(&window)
            .or_else(|| self.space.element_geometry(&window).map(|geo| geo.loc))?;

        if layer == Layer::Tiled {
            self.shell.set_layer(id, Layer::Floating);
            let th = self.title_height();
            if let Some(entry) = self.shell.find_mut(id) {
                // 记标题条位置：内容原本就在标题条下面一条。
                entry.floating_loc = Some((location.x, location.y - th));
            }
            self.arrange();
        }
        Some(location)
    }

    /// 浮动窗口移动吸附：贴到工作区边缘与相邻浮动窗口的边缘。
    ///
    /// `snap` 是最大距离（px，0 关闭）。返回吸附后的左上角。
    pub fn snap_floating(
        &self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        snap: i32,
    ) -> Point<i32, Logical> {
        let (mut dx, mut dy) = (snap + 1, snap + 1);
        let (mut x, mut y) = (rect.loc.x, rect.loc.y);
        let area = self
            .shell
            .id_of_window(window)
            .and_then(|id| self.shell.find(id))
            .and_then(|entry| self.work_areas.get(&entry.output).copied())
            .unwrap_or_else(|| self.focused_work_area());

        magnet_edge(rect.loc.x, rect.size.w, area.x, &mut dx, &mut x);
        magnet_edge(
            rect.loc.x,
            rect.size.w,
            area.x + area.width,
            &mut dx,
            &mut x,
        );
        magnet_edge(rect.loc.y, rect.size.h, area.y, &mut dy, &mut y);
        magnet_edge(
            rect.loc.y,
            rect.size.h,
            area.y + area.height,
            &mut dy,
            &mut y,
        );

        let output = self
            .shell
            .id_of_window(window)
            .and_then(|id| self.shell.find(id))
            .map(|entry| entry.output.clone())
            .unwrap_or_default();
        let visible = self.shell.visible_tags(&output);
        for other in self.space.elements() {
            if other == window {
                continue;
            }
            let Some(id) = self.shell.id_of_window(other) else {
                continue;
            };
            let Some(entry) = self.shell.find(id) else {
                continue;
            };
            if entry.output != output
                || entry.layer == Layer::Tiled
                || !tag_visible(entry.tags, entry.sticky, visible)
            {
                continue;
            }
            let Some(geometry) = self.space.element_geometry(other) else {
                continue;
            };
            if rect.loc.y < geometry.loc.y + geometry.size.h
                && geometry.loc.y < rect.loc.y + rect.size.h
            {
                magnet_edge(rect.loc.x, rect.size.w, geometry.loc.x, &mut dx, &mut x);
                magnet_edge(
                    rect.loc.x,
                    rect.size.w,
                    geometry.loc.x + geometry.size.w,
                    &mut dx,
                    &mut x,
                );
            }
            if rect.loc.x < geometry.loc.x + geometry.size.w
                && geometry.loc.x < rect.loc.x + rect.size.w
            {
                magnet_edge(rect.loc.y, rect.size.h, geometry.loc.y, &mut dy, &mut y);
                magnet_edge(
                    rect.loc.y,
                    rect.size.h,
                    geometry.loc.y + geometry.size.h,
                    &mut dy,
                    &mut y,
                );
            }
        }

        Point::from((x, y))
    }

    // ---- 平铺布局 ----

    /// 重排所有窗口：算目标矩形、映射/隐藏、给平铺窗口发 configure，最后修焦点。
    pub fn arrange(&mut self) {
        if self.outputs.is_empty() {
            return;
        }

        let th = self.title_height();
        // 先快照，后面要同时可变借用 space 与 shell。
        struct Entry {
            id: WindowId,
            window: Window,
            output: String,
            layer: Layer,
            tags: u32,
            sticky: bool,
            floating_loc: Option<(i32, i32)>,
            configured_size: Option<(i32, i32)>,
        }
        let mut entries: Vec<Entry> = self
            .shell
            .iter()
            .map(|entry| Entry {
                id: entry.id,
                window: entry.window.clone(),
                output: entry.output.clone(),
                layer: entry.layer,
                tags: entry.tags,
                sticky: entry.sticky,
                floating_loc: entry.floating_loc,
                configured_size: entry.configured_size,
            })
            .collect();

        // 先平铺层，后浮动/全屏层：同为 z=0 时后映射的在上，保证浮动窗口不被平铺窗口盖住。
        entries.sort_by_key(|entry| if entry.layer == Layer::Tiled { 0 } else { 1 });
        let mut sticky: Vec<Window> = Vec::new();
        let outputs: Vec<Output> = self.outputs.clone();

        for output in &outputs {
            let name = output.name();
            let Some(area) = self.work_areas.get(&name).copied().or_else(|| {
                self.space.output_geometry(output).map(|geo| Rect {
                    x: geo.loc.x,
                    y: geo.loc.y,
                    width: geo.size.w,
                    height: geo.size.h,
                })
            }) else {
                continue;
            };
            let full_area = self.space.output_geometry(output).map_or(area, |geo| Rect {
                x: geo.loc.x,
                y: geo.loc.y,
                width: geo.size.w,
                height: geo.size.h,
            });
            // 先让布局层更新 scroll 偏移，再产出目标矩形。
            self.shell.relayout(&name, area);
            let targets: std::collections::HashMap<WindowId, Rect> = self
                .shell
                .arrange_targets(&name, area)
                .into_iter()
                .collect();
            let visible = self.shell.visible_tags(&name);
            let other_outputs = self.other_output_areas(&name);

            for entry in entries.iter().filter(|entry| entry.output == name) {
                if !tag_visible(entry.tags, entry.sticky, visible) {
                    self.space.unmap_elem(&entry.window);
                    continue;
                }

                match entry.layer {
                    Layer::Tiled => match targets.get(&entry.id) {
                        // scroll 的行会一直向两侧延伸；滚出本输出工作区、或压到
                        // 别的输出上的列不能再留在 Space 里，否则会被相邻输出
                        // 当成本输出的窗口画出来（Smithay 按输出区域裁剪，
                        // 不看元素归属）。
                        Some(rect)
                            if rect.overlaps(area)
                                && !other_outputs.iter().any(|other| rect.overlaps(*other)) =>
                        {
                            let content = Rect {
                                x: rect.x,
                                y: rect.y + th,
                                width: rect.width.max(1),
                                height: (rect.height - th).max(1),
                            };
                            self.space.map_element(
                                entry.window.clone(),
                                (content.x, content.y),
                                false,
                            );
                            self.request_tiled_size(
                                entry.id,
                                &entry.window,
                                content,
                                entry.configured_size,
                            );
                        }
                        // 可见 tag 里但没有几何（例如正在换层），或整个目标矩形
                        // 都在本输出工作区外：先隐藏。
                        _ => self.space.unmap_elem(&entry.window),
                    },
                    Layer::Floating => {
                        // `floating_loc` 是标题条左上角；窗口内容在它下面一条。
                        let (x, y) = entry.floating_loc.unwrap_or((area.x, area.y));
                        if entry.sticky {
                            sticky.push(entry.window.clone());
                        }
                        self.space
                            .map_element(entry.window.clone(), (x, y + th), false);
                        #[cfg(feature = "xwayland")]
                        {
                            let size = entry.window.geometry().size;
                            self.sync_x11_geometry(
                                &entry.window,
                                Rectangle::new((x, y + th).into(), size),
                            );
                        }
                    }
                    Layer::Fullscreen => {
                        // 全屏窗口占满整个输出，bar 在此期间隐藏。
                        let (x, y) = entry.floating_loc.unwrap_or((full_area.x, full_area.y));
                        if entry.sticky {
                            sticky.push(entry.window.clone());
                        }
                        self.space.map_element(entry.window.clone(), (x, y), false);
                        self.request_tiled_size(
                            entry.id,
                            &entry.window,
                            Rect {
                                x,
                                y,
                                width: full_area.width.max(1),
                                height: full_area.height.max(1),
                            },
                            entry.configured_size,
                        );
                    }
                }
            }
        }

        // 键盘焦点仍然只有一个；动作作用于当前输出。
        let focused_output = self
            .output
            .as_ref()
            .map(|output| output.name())
            .or_else(|| self.outputs.first().map(|output| output.name()));
        let visible = focused_output
            .as_deref()
            .map(|output| self.shell.visible_tags(output))
            .unwrap_or(1);
        let focus = focused_output
            .as_deref()
            .and_then(|output| self.ensure_focus(output, visible));

        // 每个输出的标题条独立摆放。
        for output in &outputs {
            let name = output.name();
            let Some(area) = self.work_areas.get(&name).copied() else {
                continue;
            };
            let visible = self.shell.visible_tags(&name);
            self.place_titles(&name, area, th, visible);
        }

        if let Some(id) = focus
            && self
                .shell
                .find(id)
                .is_some_and(|entry| entry.layer != Layer::Tiled)
            && let Some(window) = self.shell.window(id).cloned()
        {
            self.space.raise_element(&window, false);
        }
        // sticky 浮动窗口永远置顶，连全屏窗口也压住。
        for window in sticky {
            self.space.raise_element(&window, false);
        }
        self.apply_focus(focus, SERIAL_COUNTER.next_serial());
        self.publish_events();
        self.request_redraw();
    }

    /// 标记「画面需要重画」。后端会在事件循环空闲时统一出帧，
    /// 同一轮里多次标记只会画一次。
    pub fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// 取走重绘请求；返回 `true` 表示这一轮该画了。
    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::replace(&mut self.needs_redraw, false)
    }

    /// 广播 IPC 状态事件；没有订阅者时只做几次比较。
    fn publish_events(&mut self) {
        if self.ipc.is_none() {
            return;
        }
        let output = self.output_name();
        let focus = self.shell.focus();
        let tags = self.shell.visible_tags(&output);
        let layout = self
            .shell
            .layout_of(&output, self.shell.focused_tag(&output))
            .name()
            .to_string();
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.publish(&output, focus, tags, &layout);
        }
    }

    /// 当前输出快照；没有输出时返回 `None`。
    pub fn output_event(&self) -> Option<crate::ipc::Event> {
        let output = self.output.as_ref()?;
        let mode = output.current_mode();
        Some(crate::ipc::Event::Output {
            output: output.name(),
            width: mode.map(|mode| mode.size.w).unwrap_or(0),
            height: mode.map(|mode| mode.size.h).unwrap_or(0),
            refresh: mode.map(|mode| mode.refresh).unwrap_or(0),
            scale: output.current_scale().fractional_scale(),
            work_x: self.work_area.x,
            work_y: self.work_area.y,
            work_width: self.work_area.width,
            work_height: self.work_area.height,
        })
    }

    /// 输出变化只在有 IPC 时算快照，没有订阅者时连比较都不做。
    fn publish_output_event(&mut self) {
        if self.ipc.is_none() {
            return;
        }
        let Some(event) = self.output_event() else {
            return;
        };
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.publish_output(event);
        }
    }

    /// 更新窗口标题缓存，并在真的变化时推 `title` 事件。
    pub fn set_window_title(&mut self, id: WindowId, title: Option<String>) {
        let Some(entry) = self.shell.find_mut(id) else {
            return;
        };
        if entry.title == title {
            return;
        }
        entry.title = title.clone();
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.publish_title(id, title.as_deref().unwrap_or(""));
        }
    }

    /// 更新窗口 app_id 缓存（查询用；不发事件）。
    pub fn set_window_app_id(&mut self, id: WindowId, app_id: Option<String>) {
        if let Some(entry) = self.shell.find_mut(id) {
            entry.app_id = app_id;
        }
    }

    /// 给平铺窗口发送尺寸 configure；尺寸没变就不打扰客户端。
    fn request_tiled_size(
        &mut self,
        id: WindowId,
        window: &Window,
        rect: Rect,
        configured_size: Option<(i32, i32)>,
    ) {
        let size = (rect.width.max(1), rect.height.max(1));
        #[cfg(feature = "xwayland")]
        if let Some(surface) = window.x11_surface() {
            if !surface.is_override_redirect() {
                let location = self.space.element_location(window).unwrap_or_default();
                let target = Rectangle::new(location, Size::from(size));
                if surface.last_configure() != target {
                    if let Err(err) = surface.configure(target) {
                        tracing::warn!(id, "配置 X11 窗口失败：{err}");
                    } else {
                        tracing::debug!(id, width = size.0, height = size.1, "X11 configure");
                    }
                }
            }
            if let Some(entry) = self.shell.find_mut(id) {
                entry.configured_size = Some(size);
            }
            return;
        }
        if configured_size == Some(size) {
            return;
        }
        let Some(toplevel) = window.toplevel() else {
            return;
        };

        toplevel.with_pending_state(|state| {
            state.size = Some(size.into());
        });
        toplevel.send_pending_configure();
        tracing::debug!(id, width = size.0, height = size.1, "平铺 configure");

        if let Some(entry) = self.shell.find_mut(id) {
            entry.configured_size = Some(size);
        }
    }

    /// 交互缩放平铺窗口：交给布局改比例，随后整轮重排。
    ///
    /// 只对平铺/浮动层生效；全屏窗口不响应缩放。
    pub fn resize_tiled_window(
        &mut self,
        id: WindowId,
        edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
        dx: f64,
        dy: f64,
    ) -> bool {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;

        let edges = ResizeEdges {
            left: matches!(
                edges,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            ),
            right: matches!(
                edges,
                ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
            ),
            top: matches!(
                edges,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            ),
            bottom: matches!(
                edges,
                ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
            ),
        };
        let Some(entry) = self.shell.find(id) else {
            return false;
        };
        if entry.layer != Layer::Tiled {
            return false;
        }
        let work = self
            .work_areas
            .get(&entry.output)
            .copied()
            .unwrap_or(self.work_area);
        if !self.shell.resize_tiled(id, work, edges, dx, dy) {
            return false;
        }
        self.arrange();
        true
    }

    /// 把浮动/全屏 X11 窗口的实际位置同步给 X 服务器。
    #[cfg(feature = "xwayland")]
    pub fn sync_x11_geometry(&self, window: &Window, rect: Rectangle<i32, Logical>) {
        let Some(surface) = window.x11_surface() else {
            return;
        };
        if surface.is_override_redirect() || surface.last_configure() == rect {
            return;
        }
        if let Err(err) = surface.configure(rect) {
            tracing::warn!("配置 X11 窗口失败：{err}");
        }
    }

    /// 当前焦点不可见时挑一个兜底窗口。
    fn ensure_focus(&mut self, output: &str, visible: u32) -> Option<WindowId> {
        // 当前焦点只要在它自己的输出上仍然可见就保留。多输出下「动作输出」
        // 和「键盘焦点所在输出」可以不同：指针移到空屏不代表要把键盘抢走。
        if let Some(id) = self.shell.focus()
            && self.shell.find(id).is_some_and(|entry| {
                tag_visible(
                    entry.tags,
                    entry.sticky,
                    self.shell.visible_tags(&entry.output),
                )
            })
        {
            return Some(id);
        }

        match self.shell.fallback_focus(output, visible) {
            Some(id) => {
                self.shell.set_focus(id);
                Some(id)
            }
            None => {
                let keep = self.shell.focus().filter(|id| {
                    self.shell.find(*id).is_some_and(|entry| {
                        tag_visible(
                            entry.tags,
                            entry.sticky,
                            self.shell.visible_tags(&entry.output),
                        )
                    })
                });
                if keep.is_none() {
                    self.shell.clear_focus();
                }
                keep
            }
        }
    }

    /// 把键盘/指针焦点、激活态与层序落到某个窗口上；`None` 表示清空焦点。
    pub fn apply_focus(&mut self, id: Option<WindowId>, serial: smithay::utils::Serial) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        // layer-shell 持有独占键盘（锁屏、菜单）时，窗口焦点变化不能把键盘抢回来。
        let layer_owns_keyboard = self
            .layer_keyboard_focus
            .as_ref()
            .is_some_and(|surface| surface.alive());

        let window = id.and_then(|id| self.shell.window(id).cloned());
        let Some(window) = window else {
            if !layer_owns_keyboard {
                keyboard.set_focus(self, None::<KeyboardFocusTarget>, serial);
            }
            return;
        };

        #[cfg(feature = "xwayland")]
        let focus = window
            .x11_surface()
            .cloned()
            .map(KeyboardFocusTarget::X11)
            .or_else(|| {
                window
                    .toplevel()
                    .map(|toplevel| KeyboardFocusTarget::Wayland(toplevel.wl_surface().clone()))
            });
        #[cfg(not(feature = "xwayland"))]
        let focus = window
            .toplevel()
            .map(|toplevel| KeyboardFocusTarget::Wayland(toplevel.wl_surface().clone()));

        if !layer_owns_keyboard {
            keyboard.set_focus(self, focus, serial);
        }

        // 激活态由 compositor 负责，否则客户端会一直画成未聚焦。
        for other in self.space.elements() {
            let activated = other == &window;
            other.set_activated(activated);
            if let Some(toplevel) = other.toplevel() {
                toplevel.send_pending_configure();
            }
        }
    }

    /// 让 layer-shell surface 独占键盘焦点（锁屏、菜单、输入法）。
    pub fn focus_layer_surface(&mut self, surface: WlSurface) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                Some(KeyboardFocusTarget::Wayland(surface.clone())),
                serial,
            );
        }
        self.layer_keyboard_focus = Some(surface);
    }

    /// layer 消失或鼠标点击到别处后，把键盘还给当前窗口。
    pub fn release_layer_focus(&mut self) {
        if self.layer_keyboard_focus.take().is_none() {
            return;
        }
        let id = self.shell.focus();
        self.apply_focus(id, SERIAL_COUNTER.next_serial());
    }

    /// 指针下要求「点击才给键盘」的 layer（`OnDemand`）。
    pub fn on_demand_layer_under(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        let output = self.output_at(pos)?;
        let origin = self.space.output_geometry(&output)?.loc.to_f64();
        let map = smithay::desktop::layer_map_for_output(&output);
        for layer in map.layers().rev() {
            let Some(geometry) = map.layer_geometry(layer) else {
                continue;
            };
            if !geometry.to_f64().contains(pos - origin) {
                continue;
            }
            if layer
                .layer_surface()
                .with_cached_state(|state| state.keyboard_interactivity)
                == KeyboardInteractivity::OnDemand
            {
                return Some(layer.wl_surface().clone());
            }
            return None;
        }
        None
    }

    /// 当前 layer 焦点是否属于 `OnDemand`（点击到别处要归还）。
    pub fn layer_focus_is_on_demand(&self) -> bool {
        let Some(focus) = self.layer_keyboard_focus.as_ref() else {
            return false;
        };
        self.outputs.iter().any(|output| {
            let map = smithay::desktop::layer_map_for_output(output);
            map.layers().any(|layer| {
                layer.wl_surface() == focus
                    && layer
                        .layer_surface()
                        .with_cached_state(|state| state.keyboard_interactivity)
                        == KeyboardInteractivity::OnDemand
            })
        })
    }
}

/// 每个客户端一份的附加数据。
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// 两个逻辑矩形的交集；没有交集时宽高为 0。
/// 配置里的 `transform "90"` / `"flipped-180"` → Smithay 的 [`Transform`]。
fn parse_transform(value: &str) -> Option<Transform> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" | "0" => Some(Transform::Normal),
        "90" => Some(Transform::_90),
        "180" => Some(Transform::_180),
        "270" => Some(Transform::_270),
        "flipped" => Some(Transform::Flipped),
        "flipped-90" => Some(Transform::Flipped90),
        "flipped-180" => Some(Transform::Flipped180),
        "flipped-270" => Some(Transform::Flipped270),
        _ => None,
    }
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0),
        height: (y1 - y0).max(0),
    }
}

/// 把窗口的起点或终点吸到 `edge`；只有比已知距离更近才生效。
fn magnet_edge(pos: i32, size: i32, edge: i32, dist: &mut i32, to: &mut i32) {
    let start = (edge - pos).abs();
    if start < *dist {
        *dist = start;
        *to = edge;
    }
    let end = (edge - (pos + size)).abs();
    if end < *dist {
        *dist = end;
        *to = edge - size;
    }
}

#[cfg(test)]
mod snap_tests {
    use super::{Rect, intersect, magnet_edge};

    #[test]
    fn intersect_keeps_overlap() {
        let a = Rect {
            x: 0,
            y: 20,
            width: 100,
            height: 80,
        };
        let b = Rect {
            x: 10,
            y: 0,
            width: 60,
            height: 60,
        };
        assert_eq!(
            intersect(a, b),
            Rect {
                x: 10,
                y: 20,
                width: 60,
                height: 40
            }
        );
    }

    #[test]
    fn intersect_without_overlap_is_empty() {
        let a = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = Rect {
            x: 20,
            y: 20,
            width: 10,
            height: 10,
        };
        let out = intersect(a, b);
        assert_eq!((out.width, out.height), (0, 0));
    }

    #[test]
    fn magnet_edge_snaps_start_and_end() {
        let (mut dist, mut to) = (10, 0);
        magnet_edge(3, 50, 0, &mut dist, &mut to);
        assert_eq!(to, 0, "起点贴左边缘");

        let (mut dist, mut to) = (10, 0);
        magnet_edge(45, 50, 100, &mut dist, &mut to);
        assert_eq!(to, 50, "终点贴右边缘");

        let (mut dist, mut to) = (5, 7);
        magnet_edge(50, 50, 0, &mut dist, &mut to);
        assert_eq!((dist, to), (5, 7), "距离太远不吸附");
    }
}

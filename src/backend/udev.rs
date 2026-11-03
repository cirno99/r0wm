//! 真机后端：DRM/KMS + libinput + libseat（`--features udev`）。
//!
//! 与 winit 后端共用同一份 [`State`]，差异只在输出来源、输入来源与呈现方式：
//! winit 把一帧画到 EGL 窗口，这里把一帧直接交给 [`DrmOutput`] 扫描到显示器。
//!
//! 当前只驱动 **一块 GPU + 一个输出**（第一个连上的连接器）。多显卡需要用
//! `MultiRenderer` 做跨 GPU 拷贝，多输出要把 `State` 改成按输出持有工作区，两者
//! 都还没做，见 `docs/parity.md` 的欠账表；其它 DRM 节点在 udev 事件里会被明确
//! 跳过并记日志。
//!
//! 渲染资源（EGL 渲染器、[`DrmOutputManager`]）放在 [`UdevData`] 里而不是闭包里：
//! dmabuf 导入回调与每个输出的渲染都要拿到渲染器，它只能从 `State` 里取。

use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode,
            compositor::{FrameError, FrameFlags, RenderFrameError},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportMemWl, element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        Window,
        space::{SpaceRenderElements, space_render_elements},
        utils::OutputPresentationFeedback,
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::Display,
    },
    render_elements,
    utils::{DeviceFd, IsAlive, Logical, Monotonic, Physical, Point, Rectangle, Scale, Time},
    wayland::{dmabuf::DmabufFeedbackBuilder, presentation::Refresh},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{
    cli::Options,
    compositor::{State, init_status_reader, run_autostart},
    config::Config,
    ui::{OverlayElement, cursor::CursorRenderElement},
};

/// 背景色：与 winit 后端保持同一个深灰。
const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.12, 1.0];
/// 按优先级尝试的扫描格式：10 bit 优先，退到 8 bit。
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// [`DrmOutput`] 的用户数据：一帧呈现后回给客户端的回执。
type UdevOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;
type UdevOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

render_elements! {
    /// 一帧的完整元素表：合成器自绘叠层在前（光标最上），窗口在后。
    pub UdevRenderElement<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Overlay=OverlayElement<GlesRenderer>,
}

/// 一个连接器对应的模式与呈现目标。
struct SurfaceData {
    output: Output,
    drm_output: UdevOutput,
    /// 已经排进队列、正在等 vblank 的帧。有值时不再重复提交，
    /// 否则一帧内容会被拆成多次 flip。
    frame_pending: bool,
    /// 上一次 flip 完成的时间；用来把下一次绘制延到帧内 0.6 处。
    last_flip: Instant,
}

/// 一块已经打开的 GPU。
pub(crate) struct DeviceData {
    pub(crate) renderer: GlesRenderer,
    manager: UdevOutputManager,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// drm 设备的 vblank 事件源；设备移除时要摘掉。
    notifier_token: RegistrationToken,
}

/// udev 后端的全部私有状态。
pub struct UdevData {
    session: LibSeatSession,
    /// 定时重绘要用；`State` 在事件循环里以 `'static` 存活。
    handle: LoopHandle<'static, State>,
    /// 我们使用的那块 GPU（DRM 主节点）。
    node: DrmNode,
    /// 设备被拔掉或会话挂起时为空。
    pub(crate) device: Option<DeviceData>,
}

impl UdevData {
    /// dmabuf 导入要用渲染器；设备不在时返回 `None`。
    pub fn renderer_mut(&mut self) -> Option<&mut GlesRenderer> {
        self.device.as_mut().map(|device| &mut device.renderer)
    }
}

pub fn run(options: Options) -> Result<()> {
    let config = Config::load(&options.config)
        .with_context(|| format!("加载配置失败：{}", options.config.display()))?;

    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().context("创建 calloop 事件循环失败")?;
    let display: Display<State> = Display::new().context("创建 Wayland display 失败")?;

    let mut state = State::new(&mut event_loop, display, config, options.config.clone());

    init_udev(&mut event_loop, &mut state)?;
    // bar 的状态行来自 stdin（基线行为）；不是终端时才接管。
    init_status_reader(&mut event_loop);

    // 子进程通过这个变量连到 r0wm，而不是宿主合成器。
    // 安全：单线程启动阶段设置，事件循环尚未并发读取环境变量。
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    }
    tracing::info!(
        socket = %state.socket_name.to_string_lossy(),
        "Wayland 套接字就绪"
    );

    run_autostart(&state);

    event_loop
        .run(None, &mut state, |state| {
            state.space.refresh();
            state.popups.cleanup();
            if let Err(err) = state.display_handle.flush_clients() {
                tracing::warn!("刷新客户端事件失败：{err}");
            }
            // 所有事件处理完之后统一出帧：没变化就不画。
            schedule_pending_frame(state);
        })
        .context("事件循环异常退出")?;

    Ok(())
}

fn init_udev(event_loop: &mut EventLoop<'static, State>, state: &mut State) -> Result<()> {
    let (session, notifier) = LibSeatSession::new().context("初始化 libseat 会话失败")?;
    let seat_name = session.seat();
    tracing::info!(seat = %seat_name, "libseat 会话就绪");

    let udev_backend = UdevBackend::new(&seat_name).context("初始化 udev 后端失败")?;

    // 主 GPU：优先取带 render node 的那个，找不到就退到第一个能打开的 DRM 节点。
    let node = primary_gpu(&seat_name)
        .with_context(|| format!("查询 seat {seat_name} 的主 GPU 失败"))?
        .and_then(|path| DrmNode::from_path(path).ok())
        .or_else(|| {
            all_gpus(&seat_name)
                .ok()?
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
        })
        .context("找不到可用的 DRM 设备（r0wm 需要 /dev/dri/card* 的读写权限）")?;
    tracing::info!(?node, "使用的主 GPU");

    let path = udev_backend
        .device_list()
        .find(|(dev_id, _)| *dev_id == node.dev_id())
        .map(|(_, path)| path)
        .context("udev 设备列表里没有主 GPU")?;

    state.udev_data = Some(UdevData {
        session: session.clone(),
        handle: event_loop.handle(),
        node,
        device: None,
    });
    open_device(state, path)?;

    // 输入：libinput 通过 libseat 拿到设备访问权。
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow::anyhow!("libinput 无法绑定 seat {seat_name}"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, state: &mut State| {
            state.process_input_event(event)
        })
        .map_err(|err| anyhow::anyhow!("注册 libinput 事件源失败：{err:?}"))?;

    // 会话切换：挂起时交出设备，恢复后重新取回并立刻重画。
    let mut resume_context = libinput_context.clone();
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, state: &mut State| match event {
            SessionEvent::PauseSession => {
                resume_context.suspend();
                pause_session(state);
            }
            SessionEvent::ActivateSession => {
                if let Err(err) = resume_context.resume() {
                    tracing::warn!("libinput 恢复失败：{err:?}");
                }
                resume_session(state);
            }
        })
        .map_err(|err| anyhow::anyhow!("注册会话事件源失败：{err:?}"))?;

    // 热插拔：只关心我们那块 GPU。
    event_loop
        .handle()
        .insert_source(udev_backend, |event, _, state: &mut State| match event {
            UdevEvent::Added { device_id, path } => {
                if is_our_device(state, device_id) {
                    if let Err(err) = open_device(state, &path) {
                        tracing::warn!("重新打开 GPU 失败：{err:#}");
                    }
                }
            }
            UdevEvent::Changed { device_id } => {
                if is_our_device(state, device_id) {
                    device_changed(state);
                }
            }
            UdevEvent::Removed { device_id } => {
                if is_our_device(state, device_id) {
                    device_removed(state);
                }
            }
        })
        .map_err(|err| anyhow::anyhow!("注册 udev 事件源失败：{err:?}"))?;

    Ok(())
}

/// `device_id` 是不是 [`UdevData::node`] 对应的设备。
fn is_our_device(state: &State, device_id: u64) -> bool {
    state
        .udev_data
        .as_ref()
        .is_some_and(|data| data.node.dev_id() == device_id)
}

/// 打开一块 GPU：会话取 fd、建 EGL 渲染器、建输出管理器，然后扫连接器。
fn open_device(state: &mut State, path: &Path) -> Result<()> {
    let (mut session, handle, node) = {
        let data = state.udev_data.as_ref().context("udev 后端尚未初始化")?;
        if data.device.is_some() {
            return Ok(());
        }
        (data.session.clone(), data.handle.clone(), data.node)
    };

    let fd = session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .with_context(|| format!("打开 DRM 设备失败：{}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, notifier) = DrmDevice::new(fd.clone(), true).context("初始化 DRM 设备失败")?;
    let gbm = GbmDevice::new(fd).context("初始化 GBM 设备失败")?;

    // 安全：GBM 是这块 GPU 自己的缓冲区管理器，EGL 显示与它同源。
    let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }.context("创建 EGL display 失败")?;
    let context = EGLContext::new(&egl_display).context("创建 EGL context 失败")?;
    // 安全：上下文刚从本线程创建，尚未交给其它线程使用。
    let renderer = unsafe { GlesRenderer::new(context) }.context("创建 GLES 渲染器失败")?;

    // 渲染器能力要如实告诉协议层，否则客户端会拿 shm/dmabuf 里不支持的格式。
    state.shm_state.update_formats(renderer.shm_formats());

    // 渲染用的节点（通常是与 card 配对的 renderD*）：直扫要按它过滤客户端缓冲。
    let render_node = EGLDevice::device_for_display(&egl_display)
        .ok()
        .and_then(|device| {
            if device.is_software() {
                tracing::warn!("EGL 设备是软件渲染，真机后端性能会很差");
            }
            device.try_get_render_node().ok().flatten()
        })
        .unwrap_or(node);

    if state.dmabuf_global.is_none() {
        let formats = renderer.dmabuf_formats();
        let feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), formats)
            .build()
            .context("构建 dmabuf feedback 失败")?;
        let global = state
            .dmabuf_state
            .create_global_with_default_feedback::<State>(&state.display_handle, &feedback);
        state.dmabuf_global = Some(global);
    }

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    // 只导出在本节点上的纹理：跨节点缓冲不能直接扫，交给合成路径。
    let exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());
    let render_formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let manager = UdevOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        render_formats,
    );

    let notifier_token = handle
        .insert_source(notifier, |event, metadata, state: &mut State| match event {
            DrmEvent::VBlank(crtc) => frame_finish(state, crtc, metadata),
            DrmEvent::Error(err) => tracing::warn!("DRM 事件错误：{err}"),
        })
        .map_err(|err| anyhow::anyhow!("注册 DRM 事件源失败：{err:?}"))?;

    let data = state.udev_data.as_mut().context("udev 后端尚未初始化")?;
    data.device = Some(DeviceData {
        renderer,
        manager,
        scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        notifier_token,
    });

    device_changed(state);
    Ok(())
}

/// 会话挂起：交出 DRM 设备，输入后端已由调用方挂起。
fn pause_session(state: &mut State) {
    let Some(device) = state
        .udev_data
        .as_mut()
        .and_then(|data| data.device.as_mut())
    else {
        return;
    };
    device.manager.pause();
}

/// 会话恢复：取回设备、重置状态，并重新安排一次渲染。
fn resume_session(state: &mut State) {
    let Some(data) = state.udev_data.as_mut() else {
        return;
    };
    let handle = data.handle.clone();
    let Some(device) = data.device.as_mut() else {
        return;
    };
    if let Err(err) = device.manager.lock().activate(false) {
        tracing::warn!("会话恢复时激活 DRM 设备失败：{err}");
        return;
    }
    for crtc in device.surfaces.keys().copied().collect::<Vec<_>>() {
        schedule_render(&handle, crtc, Duration::ZERO);
    }
}

/// 设备被拔掉：摘掉输出、停止监听，然后等下一次 `Added` 重新打开。
fn device_removed(state: &mut State) {
    let Some(device) = state.udev_data.as_mut().and_then(|data| data.device.take()) else {
        return;
    };

    let DeviceData {
        surfaces,
        notifier_token,
        ..
    } = device;
    if let Some(data) = state.udev_data.as_mut() {
        data.handle.remove(notifier_token);
    }

    for (_, surface) in surfaces {
        state.remove_output(&surface.output);
    }
    state.space.refresh();
}

/// 重新扫描连接器，按事件增删输出。
fn device_changed(state: &mut State) {
    let events = {
        let Some(device) = state
            .udev_data
            .as_mut()
            .and_then(|data| data.device.as_mut())
        else {
            return;
        };
        match device.scanner.scan_connectors(device.manager.device()) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!("扫描连接器失败：{err}");
                return;
            }
        }
    };

    for event in events {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(state, connector, crtc),
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => connector_disconnected(state, connector, crtc),
            // 连接器还连着但模式列表变了：当前不支持切模式，等重新插拔。
            DrmScanEvent::Changed { .. } => {}
            _ => {}
        }
    }
}

/// 新连接器上线：建 [`Output`]、建 DRM 表面并排一帧。
fn connector_connected(state: &mut State, connector: connector::Info, crtc: crtc::Handle) {
    // 设备先摘出来：建输出要调用 `State` 的方法，和设备借用不能同时存在。
    let Some(mut device) = state.udev_data.as_mut().and_then(|data| data.device.take()) else {
        return;
    };
    let result = setup_output(state, &mut device, connector, crtc);
    if let Some(data) = state.udev_data.as_mut() {
        data.device = Some(device);
    }
    if let Err(err) = result {
        tracing::warn!("创建输出失败：{err:#}");
    }
}

/// 真正的输出创建；调用方保证 `device` 暂时从 `State` 里摘了出来。
fn setup_output(
    state: &mut State,
    device: &mut DeviceData,
    connector: connector::Info,
    crtc: crtc::Handle,
) -> Result<()> {
    let output_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    let rule = state
        .config
        .outputs
        .iter()
        .find(|rule| rule.name == output_name);
    if rule.is_some_and(|rule| !rule.enabled) {
        tracing::info!(output = %output_name, "配置禁用了这个输出");
        return Ok(());
    }

    // 配置里的 `mode "1920x1080@60"` 优先于 PREFERRED。
    let configured_mode = rule.and_then(|rule| rule.mode.as_deref());
    let mode_id = configured_mode
        .and_then(|wanted| {
            connector
                .modes()
                .iter()
                .position(|mode| mode_matches(mode, wanted))
        })
        .or_else(|| {
            connector
                .modes()
                .iter()
                .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        })
        .unwrap_or(0);
    let drm_mode = connector
        .modes()
        .get(mode_id)
        .copied()
        .context("连接器没有可用模式")?;
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::from(connector.subpixel()),
            // EDID 解析没有打开（见 Cargo.toml），厂商与型号留空。
            make: "Unknown".into(),
            model: "Unknown".into(),
            serial_number: "Unknown".into(),
        },
    );
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), None, None, Some((0, 0).into()));

    let planes = device.manager.device().planes(&crtc).ok();
    let drm_output = device
        .manager
        .lock()
        .initialize_output(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            planes,
            &mut device.renderer,
            &DrmOutputRenderElements::<GlesRenderer, UdevRenderElement>::default(),
        )
        .map_err(|err| anyhow::anyhow!("初始化 DRM 输出失败：{err}"))?;

    // DRM 表面建好之后才登记输出：失败时不会留下一个没有扫描源的 `Output`。
    state.add_output(output.clone());
    tracing::info!(?crtc, name = %output.name(), "输出就绪");
    let handle = state
        .udev_data
        .as_ref()
        .map(|data| data.handle.clone())
        .context("udev 后端尚未初始化")?;
    device.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            drm_output,
            frame_pending: false,
            last_flip: Instant::now(),
        },
    );
    schedule_render(&handle, crtc, Duration::ZERO);
    Ok(())
}

/// `1920x1080@60` / `1920x1080` 与 DRM 模式匹配；刷新率按四舍五入比较。
fn mode_matches(mode: &smithay::reexports::drm::control::Mode, wanted: &str) -> bool {
    let Some((size, refresh)) = wanted.split_once('@') else {
        return mode.size() == parse_size(wanted).unwrap_or((0, 0));
    };
    mode.size() == parse_size(size).unwrap_or((0, 0))
        && refresh
            .parse::<f32>()
            .ok()
            .is_none_or(|refresh| (mode.vrefresh() as f32 - refresh).abs() < 0.5)
}

fn parse_size(value: &str) -> Option<(u16, u16)> {
    let (w, h) = value.split_once(['x', 'X'])?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// 连接器下线：摘掉对应输出。
fn connector_disconnected(state: &mut State, _connector: connector::Info, crtc: crtc::Handle) {
    let Some(data) = state.udev_data.as_mut() else {
        return;
    };
    let Some(device) = data.device.as_mut() else {
        return;
    };
    let Some(surface) = device.surfaces.remove(&crtc) else {
        return;
    };

    state.remove_output(&surface.output);
    state.space.refresh();
    tracing::info!(?crtc, "输出已移除");
}

/// 画一帧并排队等 vblank。空帧只安排下一次重画。
fn render(state: &mut State, crtc: crtc::Handle) {
    // 这一帧画的就是此刻的状态；安排渲染之后的变更已经被包含进来了。
    state.needs_redraw = false;
    tracing::trace!(?crtc, "按需出帧");
    // 每个 CRTC 对应自己的输出；不能拿「当前焦点输出」去画别的屏。
    let Some(output) = state
        .udev_data
        .as_ref()
        .and_then(|data| data.device.as_ref())
        .and_then(|device| device.surfaces.get(&crtc))
        .map(|surface| surface.output.clone())
    else {
        return;
    };
    // bar 内容（tag/焦点/标题）与标题条可能刚变；位图只在内容变化时重建。
    state.update_bar(&output);
    state.update_titles(&output);

    let scale = Scale::from(output.current_scale().fractional_scale());
    let integer_scale = output.current_scale().integer_scale().max(1) as u32;

    // 客户端自绘光标所依赖的 surface 销毁后要退回默认箭头，否则指针就消失了。
    if matches!(&state.cursor_status, smithay::input::pointer::CursorImageStatus::Surface(surface) if !surface.alive())
    {
        let status = smithay::input::pointer::CursorImageStatus::default_named();
        state
            .pointer_element
            .set_status(status.clone(), integer_scale);
        state.cursor_status = status;
    }

    // 光标位置是逻辑坐标，元素要放到物理坐标上；热点已经算成逻辑坐标。
    // 多输出下必须先把指针换算到**这块输出自己的逻辑坐标系**，否则同一指针
    // 会在每块屏幕上各画一遍（两块屏都从全局原点开始画）。
    let output_geo = state.space.output_geometry(&output);
    let cursor_pos = output_geo.and_then(|geometry| {
        cursor_pos_on_output(
            state.pointer_location,
            state.pointer_element.hotspot().to_f64(),
            geometry,
            scale,
        )
    });

    // 设备先摘出来：渲染要读 `State` 的其它字段（space/bar/标题条），
    // 与 `state.udev_data` 的借用不能同时存在。结束前一定放回去。
    let Some(mut device) = state.udev_data.as_mut().and_then(|data| data.device.take()) else {
        return;
    };
    render_device(state, &mut device, crtc, &output, cursor_pos, scale);
    if let Some(data) = state.udev_data.as_mut() {
        data.device = Some(device);
    }
}

/// 指针落在 `geometry` 里时，返回这块输出上的物理光标位置（热点已扣除）。
/// 不在就返回 `None`：光标只属于指针所在的那块屏。
fn cursor_pos_on_output(
    pointer: Point<f64, Logical>,
    hotspot: Point<f64, Logical>,
    geometry: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Option<Point<i32, Physical>> {
    geometry.to_f64().contains(pointer).then(|| {
        (pointer - geometry.loc.to_f64() - hotspot)
            .to_physical(scale)
            .to_i32_round()
    })
}

/// 真正的绘制；调用方保证 `device` 暂时从 `State` 里摘了出来。
fn render_device(
    state: &mut State,
    device: &mut DeviceData,
    crtc: crtc::Handle,
    output: &Output,
    cursor_pos: Option<Point<i32, Physical>>,
    scale: Scale<f64>,
) {
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return;
    };
    let renderer = &mut device.renderer;

    // 叠层顺序（前到后）：光标、托盘菜单、bar、标题条，最后是窗口。
    let mut elements: Vec<UdevRenderElement> = Vec::new();
    let cursor_elements: Vec<CursorRenderElement<GlesRenderer>> = cursor_pos
        .map(|cursor_pos| {
            state
                .pointer_element
                .render_elements(renderer, cursor_pos, scale, 1.0)
        })
        .unwrap_or_default();
    elements.extend(
        cursor_elements
            .into_iter()
            .map(|element| UdevRenderElement::Overlay(OverlayElement::Cursor(element))),
    );
    elements.extend(
        state
            .tray_popup
            .elements(renderer)
            .into_iter()
            .map(|element| UdevRenderElement::Overlay(OverlayElement::Bar(element))),
    );
    if let Some(output_geo) = state.space.output_geometry(output)
        && let Some(bar) = state.bar_for(output)
        && state.bar_visible(&output.name())
    {
        elements.extend(
            bar.elements(renderer, output_geo)
                .into_iter()
                .map(|element| UdevRenderElement::Overlay(OverlayElement::Bar(element))),
        );
    }
    if let Some(titles) = state.titles_for(output) {
        elements.extend(
            titles
                .elements(renderer)
                .into_iter()
                .map(|element| UdevRenderElement::Overlay(OverlayElement::Bar(element))),
        );
    }
    // 只画归属于本输出的窗口：跨在输出边界上的列不会被画到邻屏，
    // 但在本输出上该可见的部分仍然保留。
    let render_space = state.render_space_for_output(output);
    match space_render_elements::<_, Window, _>(renderer, [render_space.space()], output, 1.0) {
        Ok(space_elements) => {
            elements.extend(space_elements.into_iter().map(UdevRenderElement::Space));
        }
        Err(err) => {
            tracing::warn!("输出没有模式，跳过这一帧：{err}");
            return;
        }
    }

    let frame =
        surface
            .drm_output
            .render_frame(renderer, &elements, CLEAR_COLOR, FrameFlags::DEFAULT);

    match frame {
        Ok(result) => {
            if !result.is_empty {
                let feedback = state.collect_presentation_feedback(output);
                if let Err(err) = surface.drm_output.queue_frame(Some(feedback)) {
                    tracing::warn!("提交帧失败：{err}");
                } else {
                    // 帧在等 vblank；这之前不再提交新帧。
                    surface.frame_pending = true;
                }
            }

            // 关键：**空帧也要发帧回调**。动画客户端（游戏、视频、光标跟随）
            // 拿到上一帧的回调后才会提交下一帧；如果空帧只调度下一次重绘却不
            // 回调，客户端就会停止绘制，表现为画面周期性卡住，切换焦点（触发
            // configure）才恢复。anvil 的 post_repaint 同样是无条件发送。
            let elapsed = state.start_time.elapsed();
            for window in state
                .space
                .elements()
                .filter(|window| state.space.outputs_for_element(window).contains(output))
                .cloned()
                .collect::<Vec<_>>()
            {
                window.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
        }
        Err(err) => {
            if matches!(
                &err,
                RenderFrameError::PrepareFrame(FrameError::DrmError(
                    smithay::backend::drm::DrmError::DeviceInactive
                ))
            ) {
                // 会话挂起：交给 `activate` 之后重新触发渲染。
                return;
            }
            tracing::warn!("渲染失败：{err}");
        }
    }
}

/// vblank：回收上一帧的缓冲区、回呈现回执，然后安排下一帧。
fn frame_finish(state: &mut State, crtc: crtc::Handle, metadata: &mut Option<DrmEventMetadata>) {
    let Some(output) = state
        .udev_data
        .as_ref()
        .and_then(|data| data.device.as_ref())
        .and_then(|device| device.surfaces.get(&crtc))
        .map(|surface| surface.output.clone())
    else {
        return;
    };
    let Some(data) = state.udev_data.as_mut() else {
        return;
    };
    let clock = state.start_time.elapsed();
    let Some(device) = data.device.as_mut() else {
        return;
    };
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return;
    };

    let user_data = match surface.drm_output.frame_submitted() {
        Ok(user_data) => user_data,
        Err(err) => {
            // 回收失败不该让渲染停摆，下一帧继续试。
            tracing::warn!("回收帧失败：{err}");
            None
        }
    };
    // 这一帧已经上屏，下一帧可以重新排队了。
    surface.frame_pending = false;
    surface.last_flip = Instant::now();
    if let Some(Some(mut feedback)) = user_data {
        let refresh = output
            .current_mode()
            .map(|mode| Refresh::fixed(Duration::from_secs_f64(1_000.0 / mode.refresh as f64)))
            .unwrap_or(Refresh::Unknown);
        let time: Time<Monotonic> = metadata
            .as_ref()
            .and_then(|metadata| match metadata.time {
                smithay::backend::drm::DrmEventTime::Monotonic(time) => Some(time),
                smithay::backend::drm::DrmEventTime::Realtime(_) => None,
            })
            .map(Time::from)
            .unwrap_or_else(|| Time::from(clock));
        let sequence = metadata
            .as_ref()
            .map(|metadata| metadata.sequence)
            .unwrap_or(0);
        feedback.presented(
            time,
            refresh,
            sequence as u64,
            wp_presentation_feedback::Kind::Vsync,
        );
    }

    // 不在这里直接排下一帧：回到事件循环空闲回调里统一决定（
    // `schedule_pending_frame`），没变化就不出帧。
}

/// 事件循环空闲时决定要不要出帧。
///
/// 只有在画面变化（`State::needs_redraw`）且这块输出没有帧在等 vblank 时
/// 才安排一帧；空闲桌面的 GPU/CPU 占用因此会降到接近零。
///
/// 延迟取「帧内 0.6 处」：刚 flip 完立刻重画会和客户端抢同一个 vblank，
/// 把一帧的时间留给客户端重绘，端到端延迟接近一帧（anvil 同策略）。
pub fn schedule_pending_frame(state: &mut State) {
    if !state.needs_redraw {
        return;
    }
    let Some(data) = state.udev_data.as_ref() else {
        return;
    };
    let Some(device) = data.device.as_ref() else {
        return;
    };
    // 已经有帧在等 vblank：等它回来再算，别在同一帧里连排好几次。
    // 万一 vblank 事件丢了，超过两帧就重置，免得画面永久冻住。
    let stale = device
        .surfaces
        .values()
        .next()
        .map(|surface| frame_duration(&surface.output) * 2)
        .unwrap_or(Duration::from_millis(34));
    if device
        .surfaces
        .values()
        .any(|surface| surface.frame_pending && surface.last_flip.elapsed() < stale)
    {
        return;
    }
    let handle = data.handle.clone();
    let targets: Vec<(crtc::Handle, Duration)> = device
        .surfaces
        .iter()
        .map(|(crtc, surface)| {
            let delay = repaint_delay(&surface.output)
                .checked_sub(surface.last_flip.elapsed())
                .unwrap_or(Duration::ZERO);
            (*crtc, delay)
        })
        .collect();
    state.needs_redraw = false;
    for (crtc, delay) in targets {
        schedule_render(&handle, crtc, delay);
    }
}

/// 合成器在 vblank 之后等多久再画：0.6 帧。
fn repaint_delay(output: &Output) -> Duration {
    Duration::from_secs_f64(frame_duration(output).as_secs_f64() * 0.6)
}

/// 用定时器安排一次渲染；`Duration::ZERO` 表示下一个事件循环轮次。
fn schedule_render(handle: &LoopHandle<'static, State>, crtc: crtc::Handle, delay: Duration) {
    let timer = if delay.is_zero() {
        Timer::immediate()
    } else {
        Timer::from_duration(delay)
    };
    if let Err(err) = handle.insert_source(timer, move |_, _, state| {
        render(state, crtc);
        TimeoutAction::Drop
    }) {
        tracing::warn!("安排重绘失败：{err:?}");
    }
}

/// 一帧的时长；拿不到模式时按 60 Hz 兜底。
fn frame_duration(output: &Output) -> Duration {
    let refresh = output
        .current_mode()
        .map(|mode| mode.refresh)
        .filter(|refresh| *refresh > 0)
        .unwrap_or(60_000);
    // refresh 的单位是 mHz（1/1000 Hz），一帧的秒数是 1000 / refresh。
    Duration::from_secs_f64(1_000.0 / refresh as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    fn output_with_refresh(refresh: Option<i32>) -> Output {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        if let Some(refresh) = refresh {
            output.change_current_state(
                Some(WlMode {
                    size: (640, 480).into(),
                    refresh,
                }),
                None,
                None,
                None,
            );
        }
        output
    }

    /// 比较秒数并留一点浮点误差。
    fn assert_seconds(duration: Duration, expected: f64) {
        assert!(
            (duration.as_secs_f64() - expected).abs() < 1e-9,
            "实际 {duration:?}，期望 {expected} 秒"
        );
    }

    #[test]
    fn frame_duration_falls_back_to_60hz_without_a_mode() {
        assert_seconds(frame_duration(&output_with_refresh(None)), 1.0 / 60.0);
    }

    #[test]
    fn frame_duration_follows_the_output_refresh_rate() {
        // refresh 是 mHz：144000 mHz = 144 Hz。
        assert_seconds(
            frame_duration(&output_with_refresh(Some(144_000))),
            1.0 / 144.0,
        );
    }

    #[test]
    fn repaint_delay_waits_a_little_over_half_a_frame() {
        let delay = repaint_delay(&output_with_refresh(Some(60_000)));
        assert!(delay >= Duration::from_millis(9), "太短：{delay:?}");
        assert!(delay <= Duration::from_millis(11), "太长：{delay:?}");
    }

    #[test]
    fn cursor_is_localized_to_the_output_under_the_pointer() {
        // 右屏在逻辑坐标 x=2560 处。
        let geometry = Rectangle::new(Point::from((2560, 0)), Size::from((1920, 1080)));

        // 指针在左屏：右屏不画光标（这正是「两个鼠标指针」的来源）。
        assert_eq!(
            cursor_pos_on_output(
                Point::from((100.0, 100.0)),
                Point::from((10.0, 10.0)),
                geometry,
                Scale::from(1.0),
            ),
            None
        );

        // 指针在右屏：先减去输出原点，再按缩放换算到物理像素。
        assert_eq!(
            cursor_pos_on_output(
                Point::from((2660.0, 100.0)),
                Point::from((10.0, 10.0)),
                geometry,
                Scale::from(1.0),
            ),
            Some(Point::from((90, 90)))
        );
        assert_eq!(
            cursor_pos_on_output(
                Point::from((2660.0, 100.0)),
                Point::from((10.0, 10.0)),
                geometry,
                Scale::from(1.5),
            ),
            Some(Point::from((135, 135)))
        );
    }
}

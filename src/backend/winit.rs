//! 嵌套后端：Smithay 的 winit 后端，把 r0wm 跑在现有 Wayland/X11 会话里。
//!
//! 用途是开发反馈与「嵌套冒烟」测试（共识的测试方案第 ② 层）。
//! udev 后端与它共用同一份 [`State`]，差异只在输出创建与输入来源。
//!
//! 渲染器与 damage 跟踪器放在 [`State::winit_data`] 里而不是事件回调的闭包里：
//! linux-dmabuf 的导入回调必须拿到渲染器，它只能从 `State` 里取。

use std::time::Duration;

use anyhow::{Context, Result};
use smithay::{
    backend::{
        renderer::{ImportDma, ImportMemWl, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::space::render_output,
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::Display, winit::window::WindowAttributes,
    },
    utils::{IsAlive, Monotonic, Rectangle, Scale, Time, Transform},
    wayland::presentation::Refresh,
};

use crate::{
    cli::Options,
    compositor::{State, init_status_reader, run_autostart},
    config::Config,
    ui::{OverlayElement, cursor::CursorRenderElement},
};

/// 背景色：深灰，和基线 bar 的配色接近。
const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.12, 1.0];
const REFRESH_MILLIHZ: i32 = 60_000;

/// winit 后端的渲染资源；dmabuf 导入与主循环都要用。
pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
}

pub fn run(options: Options) -> Result<()> {
    let config = Config::load(&options.config)
        .with_context(|| format!("加载配置失败：{}", options.config.display()))?;

    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().context("创建 calloop 事件循环失败")?;
    let display: Display<State> = Display::new().context("创建 Wayland display 失败")?;

    let mut state = State::new(&mut event_loop, display, config, options.config.clone());

    init_winit(&mut event_loop, &mut state)?;
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
            // 有变化就在这一轮末尾重画：不依赖宿主窗口的 Redraw 事件。
            if state.needs_redraw && state.winit_data.is_some() {
                redraw(state);
            }
        })
        .context("事件循环异常退出")?;

    Ok(())
}

fn init_winit(event_loop: &mut EventLoop<State>, state: &mut State) -> Result<()> {
    let attributes = WindowAttributes::default()
        .with_title("r0wm")
        .with_surface_size(smithay::reexports::winit::dpi::LogicalSize::new(
            1280.0, 800.0,
        ));
    let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)
        .map_err(|err| anyhow::anyhow!("初始化 winit 后端失败：{err:?}"))?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: REFRESH_MILLIHZ,
    };
    // 宿主可能已经开了 HiDPI；嵌套窗口的缩放要如实转给输出。
    let scale_factor = backend.window().scale_factor();
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "r0wm".into(),
            model: "winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    // winit 的 GL 帧缓冲原点在左下角，Smithay 用 Flipped180 补偿这个 Y 翻转。
    // 用 Normal 的话，嵌套窗口里的所有内容会上下颠倒（180° 视觉旋转）。
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(OutputScale::Fractional(scale_factor)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.add_output(output.clone());

    // 渲染器的能力要如实告诉协议层，否则客户端会拿 shm/dmabuf 里不支持的格式。
    {
        let renderer = backend.renderer();
        state.shm_state.update_formats(renderer.shm_formats());
        let formats = renderer.dmabuf_formats();
        let global = state
            .dmabuf_state
            .create_global::<State>(&state.display_handle, formats);
        state.dmabuf_global = Some(global);
    }

    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.winit_data = Some(WinitData {
        backend,
        damage_tracker,
    });

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, scale_factor } => {
                if let Some(output) = state.output.clone() {
                    // 模式尺寸是物理像素，缩放单独记在输出上；布局层用逻辑坐标。
                    output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: REFRESH_MILLIHZ,
                        }),
                        None,
                        Some(OutputScale::Fractional(scale_factor)),
                        None,
                    );
                }
                state.refresh_work_area();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            // 画面由事件循环的空闲回调按需绘制：winit 的 Redraw 事件在嵌套
            // 场景下依赖宿主发帧回调，不可靠，只当作一个“可能有变化”的信号。
            WinitEvent::Redraw => state.needs_redraw = true,
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|err| anyhow::anyhow!("注册 winit 事件源失败：{err:?}"))?;

    Ok(())
}

/// 画一帧（含自绘光标），然后把呈现结果与帧回调发出去。
fn redraw(state: &mut State) {
    // 这一帧画的就是此刻的状态；画完之后的变更会重新置位。
    state.needs_redraw = false;
    tracing::trace!("按需出帧");
    let Some(output) = state.output.clone() else {
        return;
    };
    // bar 内容（tag/焦点/标题）与并口标题条可能刚变；位图只在内容变化时重建。
    state.update_bar(&output);
    state.update_titles(&output);
    let Some(mut data) = state.winit_data.take() else {
        return;
    };

    let size = data.backend.window_size();
    let damage = Rectangle::from_size(size);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let integer_scale = output.current_scale().integer_scale().max(1) as u32;

    // 客户端自绘光标所依赖的 surface 销毁后要退回默认箭头，否则指针就消失了。
    if matches!(&state.cursor_status, CursorImageStatus::Surface(surface) if !surface.alive()) {
        let status = CursorImageStatus::default_named();
        state
            .pointer_element
            .set_status(status.clone(), integer_scale);
        state.cursor_status = status;
    }

    // 光标位置是逻辑坐标，元素要放到物理坐标上；热点已经算成逻辑坐标。
    let cursor_pos = (state.pointer_location - state.pointer_element.hotspot().to_f64())
        .to_physical(scale)
        .to_i32_round();

    // 绑定失败不是致命错误（宿主窗口可能正在重建），下一帧会再试。
    let rendered = match data.backend.bind() {
        Ok((renderer, mut framebuffer)) => {
            let cursor_elements: Vec<CursorRenderElement<GlesRenderer>> = state
                .pointer_element
                .render_elements(renderer, cursor_pos, scale, 1.0);
            tracing::trace!(count = cursor_elements.len(), ?cursor_pos, "光标渲染元素");

            // 叠层顺序：先 bar 后光标，damage tracker 反序绘制，光标在最上。
            let mut overlay: Vec<OverlayElement<GlesRenderer>> = cursor_elements
                .into_iter()
                .map(OverlayElement::Cursor)
                .collect();
            // 托盘菜单在 bar 之上、光标之下：push 在两者之间。
            overlay.extend(
                state
                    .tray_popup
                    .elements(renderer)
                    .into_iter()
                    .map(OverlayElement::Bar),
            );
            if let Some(output_geo) = state.space.output_geometry(&output) {
                if let Some(bar) = state.bar_for(&output)
                    && state.bar_visible(&output.name())
                {
                    overlay.extend(
                        bar.elements(renderer, output_geo)
                            .into_iter()
                            .map(OverlayElement::Bar),
                    );
                }
            }
            if let Some(titles) = state.titles_for(&output) {
                overlay.extend(
                    titles
                        .elements(renderer)
                        .into_iter()
                        .map(OverlayElement::Bar),
                );
            }

            // 只画归属于本输出的窗口：跨在输出边界上的列不会被画到邻屏，
            // 但在本输出上该可见的部分仍然保留。
            let render_space = state.render_space_for_output(&output);
            match render_output::<_, OverlayElement<GlesRenderer>, _, _>(
                &output,
                renderer,
                &mut framebuffer,
                1.0,
                0,
                [render_space.space()],
                &overlay,
                &mut data.damage_tracker,
                CLEAR_COLOR,
            ) {
                Ok(_) => true,
                Err(err) => {
                    tracing::error!("渲染失败：{err}");
                    false
                }
            }
        }
        Err(err) => {
            tracing::error!("绑定渲染目标失败：{err}");
            false
        }
    };

    if rendered && let Err(err) = data.backend.submit(Some(&[damage])) {
        tracing::error!("提交帧失败：{err}");
    }

    state.winit_data = Some(data);

    send_presentation_feedback(state, &output);

    let elapsed = state.start_time.elapsed();
    state.space.elements().for_each(|window| {
        window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        })
    });

    state.space.refresh();
    state.popups.cleanup();
    let _ = state.display_handle.flush_clients();

    // 下一帧由事件循环的空闲回调按需请求（`needs_redraw`），
    // 静止画面不再持续重绘。
}

/// 把这一帧的呈现结果回给支持 presentation-time 的客户端。
fn send_presentation_feedback(state: &mut State, output: &Output) {
    let mut feedback = state.collect_presentation_feedback(output);
    let refresh = output
        .current_mode()
        .map(|mode| Refresh::fixed(Duration::from_secs_f64(1_000.0 / mode.refresh as f64)))
        .unwrap_or(Refresh::Unknown);
    let time: Time<Monotonic> = state.start_time.elapsed().into();
    feedback.presented(time, refresh, 0, wp_presentation_feedback::Kind::Vsync);
}

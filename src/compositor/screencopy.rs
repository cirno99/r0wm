//! wlr-screencopy-unstable-v1：截图与录屏客户端（grim/wf-recorder）的入口。
//!
//! 协议本身很薄：manager 只负责按输出 + 区域创建 frame，frame 在 `copy` 时
//! 要求合成器把当前画面写进客户端给的 shm 缓冲。r0wm 没有常驻的离屏目标，
//! 所以这里在 `copy` 请求里同步做一次完整渲染：离屏纹理 → `ReadPixels` →
//! 写进 shm。截图是低频操作，同步渲染比维护每帧快照简单得多。
//!
//! 只支持 shm 缓冲（grim 默认走 shm）；dmabuf 稍后再补，version 3 的
//! `buffer_done` 照发，客户端会退回 shm。

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::space::{SpaceRenderElements, render_output},
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
        },
    },
    render_elements,
    utils::{IsAlive, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::{
        Dispatch2, GlobalDispatch2,
        shm::{self, with_buffer_contents_mut},
    },
};

use crate::{
    compositor::State,
    ui::{OverlayElement, cursor::CursorRenderElement},
};

/// 与两个后端保持一致的背景色。
const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.12, 1.0];
/// 对外声明的协议版本；v3 才需要 `buffer_done`。
const VERSION: u32 = 3;

render_elements! {
    /// 截图的一帧：叠层在前，窗口由 `render_output` 追加在后面。
    pub CaptureElement<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Overlay=OverlayElement<GlesRenderer>,
}

/// 注册 `zwlr_screencopy_manager_v1` 全局。
pub fn init(display: &DisplayHandle) {
    display.create_global::<State, ZwlrScreencopyManagerV1, ManagerData>(VERSION, ManagerData);
}

/// manager 资源没有状态，只是请求的入口。
pub struct ManagerData;

/// frame 的用户数据。`Failed` 用于输出不存在等创建期错误。
enum FrameData {
    Failed,
    Pending {
        output: Output,
        /// 要拷贝的区域，输出本地物理像素坐标（左上角原点）。
        region: Rectangle<i32, Physical>,
        overlay_cursor: bool,
        /// 协议规定 `copy` 只能调用一次。
        copied: AtomicBool,
    },
}

impl GlobalDispatch2<ZwlrScreencopyManagerV1, State> for ManagerData {
    fn bind(
        &self,
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(manager, ManagerData);
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, State> for ManagerData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        let (new_frame, overlay_cursor, output_resource, requested) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor != 0, output, None),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                overlay_cursor != 0,
                output,
                Some((x, y, width, height)),
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        let Some((output, region)) = resolve_region(&output_resource, requested) else {
            let frame = data_init.init(new_frame, FrameData::Failed);
            frame.failed();
            return;
        };

        let size = region.size;
        let frame = data_init.init(
            new_frame,
            FrameData::Pending {
                output,
                region,
                overlay_cursor,
                copied: AtomicBool::new(false),
            },
        );

        // 只枚举 shm 缓冲；grim 默认就走这条路径。
        frame.buffer(
            wl_shm::Format::Xrgb8888,
            size.w as u32,
            size.h as u32,
            (size.w * 4) as u32,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

/// 把协议里的输出与可选区域换算成输出本地物理矩形，并裁到输出范围内。
fn resolve_region(
    output: &WlOutput,
    requested: Option<(i32, i32, i32, i32)>,
) -> Option<(Output, Rectangle<i32, Physical>)> {
    let output = Output::from_resource(output)?;
    let mode = output.current_mode()?;
    let output_rect = Rectangle::from_size(mode.size);

    let region = match requested {
        None => output_rect,
        Some((x, y, width, height)) => {
            if width <= 0 || height <= 0 {
                return None;
            }
            let scale = output.current_scale().fractional_scale();
            let logical = Rectangle::new(Point::from((x, y)), Size::from((width, height)));
            logical
                .to_physical_precise_round(scale)
                .intersection(output_rect)?
        }
    };

    (!region.is_empty()).then_some((output, region))
}

impl Dispatch2<ZwlrScreencopyFrameV1, State> for FrameData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        let FrameData::Pending {
            output,
            region,
            overlay_cursor,
            copied,
        } = self
        else {
            return;
        };

        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };

        if copied.swap(true, Ordering::SeqCst) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "copy 只能请求一次",
            );
            return;
        }

        copy_frame(
            state,
            frame,
            output,
            *region,
            *overlay_cursor,
            &buffer,
            with_damage,
        );
    }
}

/// 校验缓冲、渲染一帧并写进 shm，最后按协议顺序发 `flags`/`damage`/`ready`。
fn copy_frame(
    state: &mut State,
    frame: &ZwlrScreencopyFrameV1,
    output: &Output,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
    buffer: &WlBuffer,
    with_damage: bool,
) {
    let valid = shm::with_buffer_contents(buffer, |_ptr, _len, data| {
        data.format == wl_shm::Format::Xrgb8888
            && data.width == region.size.w
            && data.height == region.size.h
            && data.stride == region.size.w * 4
    })
    .unwrap_or(false);

    if !valid {
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
            "缓冲尺寸/格式与告知的参数不一致",
        );
        return;
    }

    let pixels = match capture_pixels(state, output, region, overlay_cursor) {
        Ok(pixels) => pixels,
        Err(err) => {
            tracing::warn!("截图渲染失败：{err:#}");
            frame.failed();
            return;
        }
    };

    let wrote = with_buffer_contents_mut(buffer, |ptr, len, data| {
        let offset = data.offset as usize;
        let stride = data.stride as usize;
        let row = region.size.w as usize * 4;
        let needed = offset + stride * region.size.h as usize;
        if len < needed || data.format != wl_shm::Format::Xrgb8888 {
            return false;
        }
        // 安全：`ptr`/`len` 是池的有效映射，`offset` 与 `needed` 已校验在范围内；
        // 写入发生在客户端请求 copy 期间，池由合成器独占访问。
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), len - offset) };
        for y in 0..region.size.h as usize {
            let src = y * row;
            let dst_off = y * stride;
            dst[dst_off..dst_off + row].copy_from_slice(&pixels[src..src + row]);
        }
        true
    })
    .unwrap_or(false);

    if !wrote {
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
            "写入截图缓冲失败",
        );
        return;
    }

    // `pixels` 已经翻成自上而下，客户端不需要再翻。
    frame.flags(Flags::empty());
    if with_damage {
        frame.damage(0, 0, region.size.w as u32, region.size.h as u32);
    }

    let elapsed = state.start_time.elapsed();
    frame.ready(
        (elapsed.as_secs() >> 32) as u32,
        (elapsed.as_secs() & 0xFFFF_FFFF) as u32,
        elapsed.subsec_nanos(),
    );
}

/// 用当前后端的渲染器截一块输出区域，返回自上而下的 ARGB/XRGB 像素。
fn capture_pixels(
    state: &mut State,
    output: &Output,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
) -> Result<Vec<u8>> {
    // 设备/后端数据临时从 `State` 里摘出来，避免渲染时同时借用 `state` 的其它字段。
    #[cfg(feature = "udev")]
    if let Some(mut device) = state.udev_data.as_mut().and_then(|data| data.device.take()) {
        let result =
            render_with_renderer(state, &mut device.renderer, output, region, overlay_cursor);
        if let Some(data) = state.udev_data.as_mut() {
            data.device = Some(device);
        }
        return result;
    }

    #[cfg(feature = "winit")]
    if let Some(mut data) = state.winit_data.take() {
        let result = match data.backend.bind() {
            Ok((renderer, _target)) => {
                render_with_renderer(state, renderer, output, region, overlay_cursor)
            }
            Err(err) => Err(anyhow!("winit 后端绑定渲染目标失败：{err}")),
        };
        state.winit_data = Some(data);
        return result;
    }

    bail!("当前后端没有可用的渲染器")
}

/// 真正的一趟离屏渲染：叠层顺序与两个后端的 `render` 一致。
fn render_with_renderer(
    state: &mut State,
    renderer: &mut GlesRenderer,
    output: &Output,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
) -> Result<Vec<u8>> {
    let mode = output.current_mode().context("输出没有当前模式")?;
    let output_size = mode.size;
    let scale = Scale::from(output.current_scale().fractional_scale());

    // bar 的 tag/标题与标题条可能刚变；截图前先刷新一次位图。
    state.update_bar(output);
    state.update_titles(output);

    if matches!(&state.cursor_status, CursorImageStatus::Surface(surface) if !surface.alive()) {
        let status = CursorImageStatus::default_named();
        state
            .pointer_element
            .set_status(status.clone(), output.current_scale().integer_scale().max(1) as u32);
        state.cursor_status = status;
    }

    // 光标是逻辑坐标；换算到这块输出自己的物理坐标。
    let cursor_pos = state.space.output_geometry(output).and_then(|geometry| {
        geometry
            .to_f64()
            .contains(state.pointer_location)
            .then(|| {
                (state.pointer_location
                    - geometry.loc.to_f64()
                    - state.pointer_element.hotspot().to_f64())
                .to_physical(scale)
                .to_i32_round()
            })
    });

    // 前到后：光标、托盘菜单、bar、标题条；窗口由 `render_output` 追加。
    let mut elements: Vec<CaptureElement> = Vec::new();
    if overlay_cursor && let Some(cursor_pos) = cursor_pos {
        let cursor_elements: Vec<CursorRenderElement<GlesRenderer>> =
            state
                .pointer_element
                .render_elements(renderer, cursor_pos, scale, 1.0);
        elements.extend(
            cursor_elements
                .into_iter()
                .map(|element| CaptureElement::Overlay(OverlayElement::Cursor(element))),
        );
    }
    elements.extend(
        state
            .tray_popup
            .elements(renderer)
            .into_iter()
            .map(|element| CaptureElement::Overlay(OverlayElement::Bar(element))),
    );
    if let Some(output_geo) = state.space.output_geometry(output)
        && let Some(bar) = state.bar_for(output)
        && state.bar_visible(&output.name())
    {
        elements.extend(
            bar.elements(renderer, output_geo)
                .into_iter()
                .map(|element| CaptureElement::Overlay(OverlayElement::Bar(element))),
        );
    }
    if let Some(titles) = state.titles_for(output) {
        elements.extend(
            titles
                .elements(renderer)
                .into_iter()
                .map(|element| CaptureElement::Overlay(OverlayElement::Bar(element))),
        );
    }

    let mut texture: GlesTexture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Argb8888, Size::from((output_size.w, output_size.h)))
            .context("创建截图离屏纹理失败")?;
    let mut target = renderer.bind(&mut texture).context("绑定截图离屏目标失败")?;
    let mut tracker = OutputDamageTracker::new(output_size, scale, Transform::Normal);
    // 与屏幕渲染用同一套归属过滤：跨在输出边界上的列不会被截进邻屏。
    let render_space = state.render_space_for_output(output);
    render_output(
        output,
        renderer,
        &mut target,
        1.0,
        0,
        [render_space.space()],
        &elements,
        &mut tracker,
        CLEAR_COLOR,
    )
    .context("渲染截图帧失败")?;

    // OpenGL 的原点在左下角：先把裁切区域翻成 GL 坐标，读完再把行倒过来。
    let gl_region = Rectangle::new(
        Point::from((region.loc.x, output_size.h - region.loc.y - region.size.h)),
        Size::from((region.size.w, region.size.h)),
    );
    let mapping = renderer
        .copy_framebuffer(&target, gl_region, Fourcc::Argb8888)
        .context("读取截图像素失败")?;
    let raw = renderer.map_texture(&mapping).context("映射截图像素失败")?;

    let row = region.size.w as usize * 4;
    let len = row * region.size.h as usize;
    if raw.len() < len {
        bail!("截图像素长度不足：{} < {len}", raw.len());
    }
    let mut pixels = vec![0u8; len];
    for y in 0..region.size.h as usize {
        let src = (region.size.h as usize - 1 - y) * row;
        pixels[y * row..(y + 1) * row].copy_from_slice(&raw[src..src + row]);
    }

    Ok(pixels)
}

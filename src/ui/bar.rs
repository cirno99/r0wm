//! 合成器自绘的 bar。
//!
//! 基线 g0wm 的 bar 是合成器自己画的（`src/bar.c`），不依赖外部客户端。
//! r0wm 照这个模型做：文本用 cosmic-text 光栅化到一块 ARGB 内存位图，
//! 再作为渲染元素叠在窗口之上；点击命中直接改合成器状态（ADR 0001 的定位 B）。
//!
//! 这一版覆盖 tags、布局名、焦点窗口标题、状态文本（含颜色转义与可点击区域，
//! 见 [`super::status`]）、通知、命令提示与托盘；托盘菜单浮层见 `super::tray`。

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    utils::{Logical, Physical, Point, Rectangle, Size, Transform},
};

use super::icon::Icon;
use super::paint::{Painter, Pixmap};
use super::status::{Color, Status};
use crate::config::{BarPosition, General, RunnerIcons};
use crate::notify::Urgency;

use std::sync::Arc;

const BG: [u8; 3] = [0x1c, 0x1c, 0x22];
const FG: [u8; 3] = [0xcc, 0xcc, 0xd2];
/// 有窗口但不可见的 tag。
const DIM: [u8; 3] = [0x7a, 0x7a, 0x86];
/// 没有窗口、也不可见的 tag。
const IDLE: [u8; 3] = [0x50, 0x50, 0x58];
const ACCENT: [u8; 3] = [0x8a, 0xb4, 0xf8];
const SEL_BG: [u8; 3] = [0x33, 0x44, 0x66];
const VISIBLE_BG: [u8; 3] = [0x2a, 0x2a, 0x36];
/// 文字左右留白（逻辑像素）。
const PAD: i32 = 8;

/// bar 需要显示的内容快照；整体相等才重画。
#[derive(Debug, Clone, PartialEq)]
pub struct BarView {
    /// 输出逻辑宽度。
    pub width: i32,
    pub scale: f64,
    pub visible_tags: u32,
    pub occupied_tags: u32,
    /// 当前焦点窗口所属的 tag 位。
    pub focused_tags: u32,
    pub layout: String,
    pub title: String,
    pub status: String,
    /// 有通知时它接管中间区域（基线把通知画在标题框里）。
    pub notify: Option<NotifyView>,
    /// 命令提示打开时它接管中间区域，优先级高于通知。
    pub runner: Option<RunnerView>,
    /// bar 右端的托盘图标；空表示不显示。
    pub systray: Vec<SystrayItem>,
}

/// 一个托盘图标的呈现参数。
#[derive(Debug, Clone)]
pub struct SystrayItem {
    /// 没有图标时画它的首字母（基线 `Id` 属性）。
    pub appid: String,
    pub icon: Option<Arc<Icon>>,
}

impl PartialEq for SystrayItem {
    fn eq(&self, other: &Self) -> bool {
        // 图标改成比指针：内容没换时不必每帧重比几万字节的像素。
        self.appid == other.appid
            && match (&self.icon, &other.icon) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

/// 通知在 bar 中间区域的呈现参数。
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyView {
    pub id: u32,
    /// 已被摊平的整行文本。
    pub text: String,
    pub urgency: Urgency,
    /// 正在浏览历史（进度条画半透明、没有倒计时）。
    pub history: bool,
    /// 历史位置（1 起）与总数。
    pub history_pos: Option<(usize, usize)>,
    /// 排队条数，显示成 `+N`。
    pub queued: usize,
    /// 剩余时间 / 总时间；0 表示不自动过期。
    pub left_ms: u32,
    pub timeout_ms: u32,
    /// 动作标签（不含 default）。
    pub actions: Vec<String>,
    /// 动作选择器当前选中项。
    pub pick: Option<usize>,
    /// `value` 提示的进度 0-100。
    pub value: Option<u8>,
    pub icon: Option<Arc<Icon>>,
    /// 文字横向滚动的字节偏移。
    pub offset: usize,
}

/// 命令提示的呈现快照。
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerView {
    /// 完整输入行（含模式前缀）。
    pub text: String,
    /// 光标在 `text` 里的字节偏移。
    pub cursor: usize,
    /// 没输入时显示的提示。
    pub placeholder: String,
    /// 光标后要补全的尾巴（动态补全）。
    pub completion: Option<String>,
    /// 算式的计算结果（画成 `= 42`）。
    pub calc: Option<String>,
    /// 当前页的候选（最多 `runner_max_candidates` 条）。
    pub candidates: Vec<RunnerCandidate>,
    /// 选中项在本页里的序号。
    pub selected: usize,
    /// 候选池里有图标（决定缺图标时要不要画占位方块）。
    pub has_icons: bool,
}

/// 命令提示里的一条候选。
#[derive(Debug, Clone)]
pub struct RunnerCandidate {
    pub label: String,
    /// App 模式下的图标；其他模式恒为 `None`。
    pub icon: Option<Arc<Icon>>,
}

impl PartialEq for RunnerCandidate {
    fn eq(&self, other: &Self) -> bool {
        // 同 [`SystrayItem`]：图标比指针，避免每帧重比几万字节像素。
        self.label == other.label
            && match (&self.icon, &other.icon) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

/// 点击落在 bar 的哪一段上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarHit {
    /// tag 位掩码。
    Tag(u32),
    Layout,
    Status,
    /// 通知正文/背景。
    Notify,
    /// 通知右侧的动作提示。
    NotifySign,
    /// 动作选择器里的第 i 个动作。
    NotifyAction(usize),
    /// 托盘里的第 i 个图标。
    Tray(usize),
}

/// 自绘 bar。每个合成器一份（当前只有一个输出）。
pub struct Bar {
    enabled: bool,
    at_top: bool,
    /// 逻辑高度；禁用时为 0。
    height: i32,

    painter: Painter,

    view: Option<BarView>,
    /// 最近一次光栅化时解析出的状态行（点击命令从这里取）。
    status: Status,
    /// 每个状态片段在 bar 内的物理像素区间 `[x0, x1)`。
    status_ranges: Vec<(i32, i32)>,
    buffer: Option<MemoryRenderBuffer>,
    /// 位图尺寸（物理像素）。
    buffer_size: (i32, i32),
    /// 命中区，输出内逻辑坐标。
    hits: Hits,
    /// 通知进度条宽度（逻辑像素，0 关闭）。
    notify_line_width: i32,
    notify_line_radius: i32,
    notify_action_sign: String,
    /// 上一次绘制通知时正文可用宽度（物理像素），供翻页滚动使用。
    notify_text_width: i32,
    /// 是否显示右端托盘。
    show_systray: bool,
    /// 托盘图标之间的间距与两端内边距（逻辑像素）。
    systray_spacing: i32,
    systray_padding: i32,
    /// 托盘图标边长（逻辑像素，0 表示填满 bar）。
    systray_icon_size: u32,

    /// runner 的图标显示方式。
    runner_icons: RunnerIcons,
    /// 候选图标边长（逻辑像素，0 用行高）。
    runner_icon_size: u32,
    /// 缺图标占位方块的圆角比例。
    runner_icon_round: f32,
    /// 缺图标占位方块的边框宽度（逻辑像素）。
    runner_icon_border: u32,
    /// 选中候选胶囊高度（逻辑像素，0 用 4/3 行高）。
    runner_pill_height: u32,
    /// 选中候选胶囊左右内边距（逻辑像素，0 自动）。
    runner_pill_padding: u32,
    /// 选中候选胶囊圆角比例（负数按 1 处理）。
    runner_pill_round: f32,
}

/// 命中区：矩形 + 点击段。
type Hits = Vec<(Rectangle<i32, Logical>, BarHit)>;

impl Bar {
    pub fn new(general: &General) -> Self {
        let font_size = general.font_size.max(6.0);
        let line_height = font_size * 1.25;
        // 基线 `barheight` 是字体行高的倍数，0 表示自动。
        let multiplier = if general.bar_height > 0.0 {
            general.bar_height
        } else {
            1.1
        };
        let height = (line_height * multiplier).round().max(font_size + 6.0) as i32;

        Self {
            enabled: general.bar_enabled,
            at_top: general.bar_position == BarPosition::Top,
            height: if general.bar_enabled { height } else { 0 },
            painter: Painter::new(font_size, line_height, general.font_family.clone()),
            view: None,
            status: Status::default(),
            status_ranges: Vec::new(),
            buffer: None,
            buffer_size: (0, 0),
            hits: Vec::new(),
            notify_line_width: general.notification_line_width as i32,
            notify_line_radius: general.notification_line_radius as i32,
            notify_action_sign: general.notification_action_sign.clone(),
            notify_text_width: 0,
            show_systray: general.show_systray,
            systray_spacing: general.systray_spacing,
            systray_padding: general.systray_padding,
            systray_icon_size: general.systray_icon_size,
            runner_icons: general.runner_icons,
            runner_icon_size: general.runner_icon_size,
            runner_icon_round: general.runner_icon_round,
            runner_icon_border: general.runner_icon_border,
            runner_pill_height: general.runner_pill_height,
            runner_pill_padding: general.runner_pill_padding,
            runner_pill_round: general.runner_pill_round,
        }
    }

    /// 热重载后更新通知相关设置；强制下一帧重建位图。
    pub fn reconfigure(&mut self, general: &General) {
        self.notify_line_width = general.notification_line_width as i32;
        self.notify_line_radius = general.notification_line_radius as i32;
        self.notify_action_sign = general.notification_action_sign.clone();
        self.show_systray = general.show_systray;
        self.systray_spacing = general.systray_spacing;
        self.systray_padding = general.systray_padding;
        self.systray_icon_size = general.systray_icon_size;
        self.runner_icons = general.runner_icons;
        self.runner_icon_size = general.runner_icon_size;
        self.runner_icon_round = general.runner_icon_round;
        self.runner_icon_border = general.runner_icon_border;
        self.runner_pill_height = general.runner_pill_height;
        self.runner_pill_padding = general.runner_pill_padding;
        self.runner_pill_round = general.runner_pill_round;
        self.view = None;
    }

    /// 占用工作区的逻辑高度；禁用时为 0。
    pub fn height(&self) -> i32 {
        self.height
    }

    /// 是否贴在输出顶部。
    pub fn at_top(&self) -> bool {
        self.at_top
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 命中测试；`pos` 是相对 bar 左上角的逻辑坐标。
    pub fn hit(&self, pos: Point<f64, Logical>) -> Option<BarHit> {
        if !self.enabled {
            return None;
        }
        self.hits
            .iter()
            .find(|(rect, _)| rect.to_f64().contains(pos))
            .map(|(_, hit)| *hit)
    }

    /// 状态文本在 `pos`（bar 内逻辑坐标）处、按 `button` 触发的命令。
    ///
    /// 状态行沿用基线的转义语法（`^sN!命令^`），见 [`super::status`]。
    pub fn status_command(&self, pos: Point<f64, Logical>, button: u8) -> Option<&str> {
        let x = (pos.x * self.painter.scale()).round() as i32;
        let index = self
            .status_ranges
            .iter()
            .position(|(x0, x1)| x >= *x0 && x < *x1)?;
        self.status.command(index, button)
    }

    /// 内容变了就重画；返回值表示是否真的重建了位图。
    pub fn update(&mut self, view: BarView) -> bool {
        if !self.enabled || self.height <= 0 {
            return false;
        }
        if self.view.as_ref() == Some(&view) {
            return false;
        }
        self.view = Some(view);
        self.rebuild();
        true
    }

    /// 把 bar 作为渲染元素交出去；`output_geo` 是输出在全局逻辑坐标里的矩形。
    pub fn elements<R>(
        &self,
        renderer: &mut R,
        output_geo: Rectangle<i32, Logical>,
    ) -> Vec<MemoryRenderBufferRenderElement<R>>
    where
        R: ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let (Some(buffer), Some(view)) = (self.buffer.as_ref(), self.view.as_ref()) else {
            return Vec::new();
        };
        let (phys_w, phys_h) = self.buffer_size;
        let bar_y = if self.at_top {
            output_geo.loc.y
        } else {
            output_geo.loc.y + output_geo.size.h - self.height
        };
        let location = Point::<f64, Physical>::from((
            output_geo.loc.x as f64 * view.scale,
            bar_y as f64 * view.scale,
        ));
        // 位图按物理像素光栅化，`scale` 固定 1；显式给 src 覆盖整张位图，
        // 逻辑尺寸另算，这样分数缩放下也不会采样错位。
        let src = Rectangle::<f64, Logical>::from_size(Size::from((phys_w as f64, phys_h as f64)));
        let size = Size::<i32, Logical>::from((view.width, self.height));

        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            Some(1.0),
            Some(src),
            Some(size),
            Kind::Unspecified,
        ) {
            Ok(element) => vec![element],
            Err(err) => {
                tracing::warn!("bar 位图导入失败：{err}");
                Vec::new()
            }
        }
    }

    // ---- 光栅化 ----

    fn rebuild(&mut self) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let (pixmap, hits) = self.rasterize(&view);
        if let Ok(path) = std::env::var("R0WM_BAR_DUMP") {
            if let Err(err) = std::fs::write(&path, pixmap.to_ppm()) {
                tracing::warn!(path, "bar 位图导出失败：{err}");
            }
        }
        let (phys_w, phys_h) = (pixmap.width, pixmap.height);
        let opaque = vec![Rectangle::from_size(Size::from((phys_w, phys_h)))];
        self.buffer = Some(MemoryRenderBuffer::from_slice(
            &pixmap.data,
            Fourcc::Argb8888,
            (phys_w, phys_h),
            1,
            Transform::Normal,
            Some(opaque),
        ));
        self.buffer_size = (phys_w, phys_h);
        self.hits = hits;
    }

    /// 把视图光栅化成位图与命中区；拆出来便于单测。
    fn rasterize(&mut self, view: &BarView) -> (Pixmap, Hits) {
        let scale = view.scale.max(0.1);
        self.painter.set_scale(scale);
        let phys_w = ((view.width as f64 * scale).round() as i32).max(1);
        let phys_h = ((self.height as f64 * scale).round() as i32).max(1);
        let pad = ((PAD as f64 * scale).round() as i32).max(1);
        let line_h = (self.painter.line_height() as f64 * scale).round() as f32;
        let y_off = ((phys_h as f32 - line_h) / 2.0).round() as i32;

        let mut pixmap = Pixmap::new(phys_w, phys_h, BG);
        let mut hits = Vec::new();

        // 左：9 个 tag。可见的用底色块，焦点 tag 用强调色。
        let mut x = pad;
        for tag in 0..9u32 {
            let mask = 1u32 << tag;
            let label = (tag + 1).to_string();
            let text_w = self.text_width(&label);
            let cell_w = text_w + pad * 2;

            let (bg, fg) = if view.focused_tags & mask != 0 && view.visible_tags & mask != 0 {
                (SEL_BG, [0xff, 0xff, 0xff])
            } else if view.visible_tags & mask != 0 {
                (VISIBLE_BG, FG)
            } else if view.occupied_tags & mask != 0 {
                (BG, DIM)
            } else {
                (BG, IDLE)
            };
            if bg != BG {
                pixmap.fill_rect(x, 0, cell_w, phys_h, bg);
            }
            let fg = [fg[0], fg[1], fg[2], 0xff];
            self.draw_text(&mut pixmap, &label, x + pad, y_off, (0, phys_w), fg);
            hits.push((
                physical_rect(x, 0, cell_w, phys_h, scale),
                BarHit::Tag(mask),
            ));
            x += cell_w;
        }

        // 布局名紧跟 tags，用强调色。
        let layout = format!("[{}]", view.layout);
        let layout_w = self.text_width(&layout);
        let accent = [ACCENT[0], ACCENT[1], ACCENT[2], 0xff];
        self.draw_text(&mut pixmap, &layout, x + pad, y_off, (0, phys_w), accent);
        hits.push((
            physical_rect(x, 0, layout_w + pad * 2, phys_h, scale),
            BarHit::Layout,
        ));
        x += layout_w + pad * 2;

        // 右：状态文本。每段各自的前景/背景色，区间记录下来供点击命中。
        let status = Status::parse(&view.status);
        let mut status_ranges = Vec::new();
        let tray_w = self.systray_width(&view.systray, scale);
        let mut right = phys_w - pad;
        if tray_w > 0 {
            right = (right - tray_w).max(x + pad);
        }
        if !status.items.is_empty() {
            let widths: Vec<i32> = status
                .items
                .iter()
                .map(|item| self.text_width(&item.text))
                .collect();
            let total: i32 = widths.iter().sum();
            let status_x = (right - total).max(x + pad);
            let mut px = status_x;
            for (item, width) in status.items.iter().zip(&widths) {
                if px >= right {
                    break;
                }
                if let Some(bg) = item.bg {
                    pixmap.blend_rect(px, 0, *width, phys_h, bg);
                }
                let fg = item.fg.unwrap_or([FG[0], FG[1], FG[2], 0xff]);
                self.draw_text(&mut pixmap, &item.text, px, y_off, (status_x, right), fg);
                let x1 = (px + width).min(right);
                status_ranges.push((px, x1));
                px += width;
            }
            hits.push((
                physical_rect(status_x, 0, right - status_x, phys_h, scale),
                BarHit::Status,
            ));
            right = status_x - pad;
        }
        self.status = status;
        self.status_ranges = status_ranges;

        // 中：命令提示优先，其次是通知，最后才是焦点窗口标题。
        if let Some(runner) = &view.runner {
            self.draw_runner(&mut pixmap, runner, x + pad, right, phys_h, y_off, scale);
        } else if let Some(notify) = &view.notify {
            self.draw_notify(
                &mut pixmap,
                notify,
                x + pad,
                right,
                phys_h,
                y_off,
                scale,
                &mut hits,
            );
        } else if !view.title.is_empty() && right > x + pad {
            let title_w = self.text_width(&view.title);
            let avail = right - (x + pad);
            let title_x = if title_w <= avail {
                x + pad + (avail - title_w) / 2
            } else {
                x + pad
            };
            self.draw_text(
                &mut pixmap,
                &view.title,
                title_x,
                y_off,
                (x + pad, right),
                [FG[0], FG[1], FG[2], 0xff],
            );
        }

        // 右端：托盘图标。先于返回，命中区按图标格记录。
        self.draw_systray(&mut pixmap, &view.systray, phys_h, y_off, scale, &mut hits);

        (pixmap, hits)
    }

    /// 托盘占用的物理像素宽度；不显示时为 0。
    fn systray_width(&self, items: &[SystrayItem], scale: f64) -> i32 {
        if !self.show_systray || items.is_empty() {
            return 0;
        }
        let count = items.len() as f64;
        let icon = self.systray_icon_logical() as f64;
        let spacing = self.systray_spacing as f64;
        let padding = self.systray_padding as f64;
        ((count * icon + (count - 1.0) * spacing + 2.0 * padding) * scale).round() as i32
    }

    fn systray_icon_logical(&self) -> i32 {
        if self.systray_icon_size == 0 {
            self.height
        } else {
            self.systray_icon_size as i32
        }
    }

    /// 画右端托盘：图标缩进格子，没有图标就用 appid 的首字母。
    fn draw_systray(
        &mut self,
        pixmap: &mut Pixmap,
        items: &[SystrayItem],
        phys_h: i32,
        y_off: i32,
        scale: f64,
        hits: &mut Hits,
    ) {
        let width = self.systray_width(items, scale);
        if width <= 0 {
            return;
        }
        let icon_size = ((self.systray_icon_logical() as f64) * scale)
            .round()
            .max(1.0) as i32;
        let spacing = ((self.systray_spacing as f64) * scale).round() as i32;
        let padding = ((self.systray_padding as f64) * scale).round() as i32;
        let mut x = pixmap.width - width + padding;

        for (index, item) in items.iter().enumerate() {
            if let Some(icon) = &item.icon
                && icon.width > 0
                && icon.height > 0
            {
                // 先在图标空间里双线性缩放，再原尺寸贴到画布上。
                let fitted = super::icon::fit(
                    &icon.rgba,
                    icon.width as u32,
                    icon.height as u32,
                    icon_size as u32,
                );
                pixmap.blit_rgba(
                    x + (icon_size - fitted.width) / 2,
                    (phys_h - fitted.height) / 2,
                    fitted.width,
                    fitted.height,
                    &fitted,
                );
            } else if let Some(letter) = item.appid.chars().find(|c| !c.is_whitespace()) {
                let text = letter.to_string();
                let text_w = self.text_width(&text);
                self.draw_text(
                    pixmap,
                    &text,
                    x + (icon_size - text_w) / 2,
                    y_off,
                    (x, x + icon_size),
                    [FG[0], FG[1], FG[2], 0xff],
                );
            }
            hits.push((
                physical_rect(x, 0, icon_size, phys_h, scale),
                BarHit::Tray(index),
            ));
            x += icon_size + spacing;
        }
    }

    /// 画通知：左侧进度条、图标、正文、动作提示/选择器、历史位置。
    ///
    /// 坐标全是物理像素；返回值不直接用，命中区追加到 `hits`。
    #[allow(clippy::too_many_arguments)]
    fn draw_notify(
        &mut self,
        pixmap: &mut Pixmap,
        view: &NotifyView,
        left: i32,
        right: i32,
        phys_h: i32,
        y_off: i32,
        scale: f64,
        hits: &mut Hits,
    ) {
        if right <= left {
            return;
        }
        let (bg, fg, line) = match view.urgency {
            Urgency::Low => (BG, [0xff, 0xff, 0xff], [0x2e, 0xc2, 0x7e]),
            Urgency::Critical => (BG, [0xff, 0xff, 0xff], [0xe0, 0x1b, 0x24]),
            Urgency::Normal => (BG, [0xff, 0xff, 0xff], [0xe5, 0xa5, 0x0a]),
        };
        // 基线给通知自己的底色，和 bar 其余部分区分开。
        pixmap.fill_rect(left, 0, right - left, phys_h, [0x00, 0x00, 0x00]);
        let pad = ((PAD as f64 * scale).round() as i32).max(1) / 2;
        let line_h = (self.painter.line_height() as f64 * scale).round() as i32;
        let mut cx = left + pad;
        let full = phys_h;

        // 左缘的倒计时/进度竖条。
        let line_w = ((self.notify_line_width as f64) * scale).round() as i32;
        if self.notify_line_width > 0 && line_w > 0 {
            let len = if view.history || view.timeout_ms == 0 {
                full
            } else {
                ((full as u64 * view.left_ms as u64) / view.timeout_ms.max(1) as u64) as i32
            };
            let alpha = if view.history { 0x80 } else { 0xff };
            let color = [line[0], line[1], line[2], alpha];
            let radius = (self.notify_line_radius as f64 * scale).min(line_w as f64 / 2.0);
            if radius > 0.5 && len > 2 {
                let cx = cx as f64 + line_w as f64 / 2.0;
                pixmap.fill_circle(cx, (full - len) as f64 + radius, radius, color);
                pixmap.fill_circle(cx, full as f64 - radius, radius, color);
                pixmap.blend_rect(
                    cx as i32 - line_w / 2,
                    full - len + radius as i32,
                    line_w,
                    len - (radius as i32) * 2,
                    color,
                );
            } else {
                pixmap.blend_rect(cx, full - len, line_w, len, color);
            }
            cx += line_w + pad;
        }

        // 图标按行高缩放。
        if let Some(icon) = &view.icon
            && icon.width > 0
            && icon.height > 0
        {
            let height = line_h.max(1);
            let width = (icon
                .width
                .saturating_mul(height)
                .checked_div(icon.height)
                .unwrap_or(height))
            .max(1);
            pixmap.blit_rgba(cx, (full - height) / 2, width, height, icon);
            cx += width + pad;
        }

        let mut end = right - pad;
        // 右侧：历史位置或排队数。
        let count = if let Some((pos, total)) = view.history_pos {
            format!("{pos}/{total}")
        } else if view.queued > 0 {
            format!("+{}", view.queued)
        } else {
            String::new()
        };
        if !count.is_empty() {
            let width = self.text_width(&count);
            if end - width > cx {
                let count_x = end - width;
                self.draw_text(
                    pixmap,
                    &count,
                    count_x,
                    y_off,
                    (count_x, end),
                    [0xaa, 0xaa, 0xb4, 0xff],
                );
                end = count_x - pad;
            }
        }

        // 右侧：有动作且没打开选择器时的提示；打开时改画动作芯片。
        let picking = view.pick.is_some();
        let sign = if !picking && !view.actions.is_empty() {
            self.notify_action_sign.clone()
        } else {
            String::new()
        };
        if !sign.is_empty() {
            let width = self.text_width(&sign);
            if end - width > cx {
                let sign_x = end - width;
                self.draw_text(
                    pixmap,
                    &sign,
                    sign_x,
                    y_off,
                    (sign_x, end),
                    [0xaa, 0xaa, 0xb4, 0xff],
                );
                hits.push((
                    Rectangle::new(
                        Point::from((self.logical(sign_x - pad, scale), 0)),
                        Size::from((
                            self.logical(width + pad * 2, scale),
                            self.logical(phys_h, scale),
                        )),
                    ),
                    BarHit::NotifySign,
                ));
                end = sign_x - pad;
            }
        }
        self.notify_text_width = (end - cx).max(0);

        if let Some(selected) = view.pick {
            let mut chip_x = cx;
            for (index, label) in view.actions.iter().enumerate() {
                let width = self.text_width(label) + pad * 2;
                if chip_x + width > end {
                    break;
                }
                if index == selected {
                    pixmap.fill_rect(chip_x, 0, width, phys_h, SEL_BG);
                }
                self.draw_text(
                    pixmap,
                    label,
                    chip_x + pad,
                    y_off,
                    (chip_x, chip_x + width),
                    [fg[0], fg[1], fg[2], 0xff],
                );
                let rect = Rectangle::new(
                    Point::from((self.logical(chip_x, scale), 0)),
                    Size::from((self.logical(width, scale), self.logical(phys_h, scale))),
                );
                hits.push((rect, BarHit::NotifyAction(index)));
                chip_x += width + pad;
            }
        } else {
            // 正文：`value` 提示把左侧按百分比染成进度色。
            if let Some(value) = view.value {
                let tint_w = ((end - cx).max(0) as u32 * value as u32 / 100) as i32;
                pixmap.fill_rect(cx, 0, tint_w, phys_h, mix(bg, line, 1, 3));
            }
            let text = view.text.get(view.offset..).unwrap_or("");
            let avail = (end - cx).max(0);
            let arrow = "›";
            let arrow_w = self.text_width(arrow);
            let full_w = self.text_width(text);
            let (shown, cut) = if full_w <= avail || avail <= arrow_w {
                (text.to_string(), false)
            } else {
                let bytes = self.fit_prefix(text, avail - arrow_w);
                (text[..bytes].to_string(), bytes < text.len())
            };
            if !shown.is_empty() {
                self.draw_text(
                    pixmap,
                    &shown,
                    cx,
                    y_off,
                    (cx, end),
                    [fg[0], fg[1], fg[2], 0xff],
                );
            }
            if cut && arrow_w <= avail {
                self.draw_text(
                    pixmap,
                    arrow,
                    end - arrow_w,
                    y_off,
                    (end - arrow_w, end),
                    [fg[0], fg[1], fg[2], 0xff],
                );
            }
        }

        // 整条通知的点击区；放在最后，具体的芯片/提示命中优先。
        hits.push((
            Rectangle::new(
                Point::from((self.logical(left, scale), 0)),
                Size::from((
                    self.logical(right - left, scale),
                    self.logical(phys_h, scale),
                )),
            ),
            BarHit::Notify,
        ));
    }

    /// 画命令提示：图标位、输入行（带光标）、补全/计算结果，后面紧跟候选。
    ///
    /// 与基线 `drawselbar` 一样，提示和候选共用中间那一格，超出就截断。
    #[allow(clippy::too_many_arguments)]
    fn draw_runner(
        &mut self,
        pixmap: &mut Pixmap,
        view: &RunnerView,
        left: i32,
        right: i32,
        phys_h: i32,
        y_off: i32,
        scale: f64,
    ) {
        if right <= left {
            return;
        }
        let line_h = (self.painter.line_height() as f64 * scale).round() as i32;
        let text_h = line_h.max(1);
        pixmap.fill_rect(left, 0, right - left, phys_h, [0x00, 0x00, 0x00]);

        // 胶囊几何（基线 `drawcandidates`）：高度默认 4/3 行高，夹在行高与
        // bar 高之间；圆角按高度的一半再乘比例。
        let pill_h = if self.runner_pill_height > 0 {
            ((self.runner_pill_height as f64) * scale).round() as i32
        } else {
            text_h * 4 / 3
        }
        .clamp(text_h, phys_h.max(text_h));
        let round = if self.runner_pill_round < 0.0 {
            1.0
        } else {
            self.runner_pill_round.clamp(0.0, 1.0)
        };
        let radius = pill_h as f64 / 2.0 * round as f64;

        let mut pad = if self.runner_pill_padding > 0 {
            ((self.runner_pill_padding as f64) * scale).round() as i32
        } else {
            ((PAD as f64 * scale).round() as i32).max(1) / 2
        };
        // 圆角会压到文字，内边距至少要避开圆弧。
        let margin = (pill_h - text_h) / 2;
        if (margin as f64) < radius && radius > 0.0 {
            let arc = radius
                - (radius * radius - (radius - margin as f64).powi(2))
                    .max(0.0)
                    .sqrt();
            pad = pad.max(arc.ceil() as i32);
        }
        pad = pad.max(1);

        let icon_size = if self.runner_icon_size > 0 {
            ((self.runner_icon_size as f64) * scale).round() as i32
        } else {
            text_h
        }
        .min(pill_h);

        let mut cx = left + pad;

        // 左侧图标位：显示方式不是 none 时始终留出同样宽的空档，文字不会跳。
        if self.runner_icons != RunnerIcons::None && icon_size + pad < right - left {
            let icon = view
                .candidates
                .get(view.selected)
                .and_then(|candidate| candidate.icon.as_deref());
            match icon {
                Some(icon) => self.draw_runner_icon(pixmap, icon, cx, icon_size, phys_h),
                None => {
                    self.draw_icon_holder(pixmap, cx, (phys_h - icon_size) / 2, icon_size, scale)
                }
            }
            cx += icon_size + pad;
        }

        let text_x = cx;
        let text_w = self.text_width(&view.text);
        if !view.text.is_empty() {
            self.draw_text(
                pixmap,
                &view.text,
                text_x,
                y_off,
                (text_x, right),
                [0xff, 0xff, 0xff, 0xff],
            );
        }

        // 光标：只量光标之前的宽度，高度和圆角跟着字体走。
        let caret_w = (text_h / 12).max(2);
        let caret_h = (text_h - 2 * (text_h / 9)).max(2);
        let before = view
            .text
            .get(..view.cursor.min(view.text.len()))
            .unwrap_or("");
        let caret_x = text_x + self.text_width(before);
        if caret_x + caret_w <= right {
            pixmap.fill_round_rect(
                caret_x,
                y_off + (text_h - caret_h) / 2,
                caret_w,
                caret_h,
                caret_w as f64 / 2.0,
                [0xff, 0xff, 0xff, 0xff],
            );
        }

        let mut after = (text_x + text_w + caret_w).min(right);
        if view.text.is_empty() {
            if !view.placeholder.is_empty() {
                self.draw_text(
                    pixmap,
                    &view.placeholder,
                    text_x,
                    y_off,
                    (text_x, right),
                    [0x88, 0x88, 0x94, 0xff],
                );
            }
        } else if let Some(calc) = &view.calc {
            let text = format!("= {calc}");
            self.draw_text(
                pixmap,
                &text,
                after,
                y_off,
                (after, right),
                [0x88, 0x88, 0x94, 0xff],
            );
            after += self.text_width(&text);
        } else if let Some(tail) = &view.completion {
            self.draw_text(
                pixmap,
                tail,
                after,
                y_off,
                (after, right),
                [0x88, 0x88, 0x94, 0xff],
            );
            after += self.text_width(tail);
        }

        // 候选：选中项套一个和 tag 选中态同色的胶囊，左端可以带图标。
        let mut px = (after + pad).min(right);
        for (index, candidate) in view.candidates.iter().enumerate() {
            let icon = if self.runner_icons == RunnerIcons::All {
                candidate.icon.as_deref()
            } else {
                None
            };
            let holder = self.runner_icons == RunnerIcons::All && icon.is_none() && view.has_icons;
            let iw = if icon.is_some() || holder {
                icon_size + pad / 2
            } else {
                0
            };
            let tw = self
                .text_width(&candidate.label)
                .min(right - px - 2 * pad - iw);
            if tw <= 0 {
                break;
            }
            let width = iw + tw + 2 * pad;
            if index == view.selected {
                pixmap.fill_round_rect(
                    px,
                    (phys_h - pill_h) / 2,
                    width,
                    pill_h,
                    radius,
                    [SEL_BG[0], SEL_BG[1], SEL_BG[2], 0xff],
                );
            }
            if let Some(icon) = icon {
                self.draw_runner_icon(pixmap, icon, px + pad, icon_size, phys_h);
            } else if holder {
                self.draw_icon_holder(pixmap, px + pad, (phys_h - icon_size) / 2, icon_size, scale);
            }
            let color = if index == view.selected {
                [0xff, 0xff, 0xff, 0xff]
            } else {
                [0xaa, 0xaa, 0xb4, 0xff]
            };
            let text_at = px + pad + iw;
            self.draw_text(
                pixmap,
                &candidate.label,
                text_at,
                y_off,
                (text_at, text_at + tw),
                color,
            );
            px += width;
        }
    }

    /// 按比例缩放图标并画在 `size` 方框里（方框在 bar 内垂直居中）。
    fn draw_runner_icon(&self, pixmap: &mut Pixmap, icon: &Icon, x: i32, size: i32, phys_h: i32) {
        if size <= 0 || icon.width <= 0 || icon.height <= 0 {
            return;
        }
        let (w, h) = if icon.width >= icon.height {
            (size, (icon.height * size / icon.width).max(1))
        } else {
            ((icon.width * size / icon.height).max(1), size)
        };
        pixmap.blit_rgba(x + (size - w) / 2, (phys_h - h) / 2, w, h, icon);
    }

    /// 缺图标时的圆角方块占位（基线 `drawiconholder`）。
    fn draw_icon_holder(&self, pixmap: &mut Pixmap, x: i32, y: i32, size: i32, scale: f64) {
        if size <= 0 {
            return;
        }
        let border = ((self.runner_icon_border as f64) * scale).round() as i32;
        let border = border.min(size / 2).max(0);
        let round = self.runner_icon_round.clamp(0.0, 1.0);
        let radius = size as f64 / 2.0 * round as f64;
        if border > 0 {
            pixmap.fill_round_rect(x, y, size, size, radius, [0x4a, 0x4a, 0x4a, 0xff]);
        }
        let inner = (size - 2 * border).max(1);
        pixmap.fill_round_rect(
            x + border,
            y + border,
            inner,
            inner,
            (radius - border as f64).max(0.0),
            [0x2e, 0x2e, 0x2e, 0xff],
        );
    }

    /// 把物理像素换回逻辑单位（命中区用）。
    fn logical(&self, px: i32, scale: f64) -> i32 {
        (px as f64 / scale.max(0.1)).round() as i32
    }

    /// 最多能放下 `width` 物理像素的字节前缀（只切在码点边界）。
    fn fit_prefix(&mut self, text: &str, width: i32) -> usize {
        let mut lo = 0usize;
        let mut hi = text.chars().count();
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let bytes = text
                .char_indices()
                .nth(mid)
                .map(|(index, _)| index)
                .unwrap_or(text.len());
            if self.text_width(&text[..bytes]) <= width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        text.char_indices()
            .nth(lo)
            .map(|(index, _)| index)
            .unwrap_or(text.len())
    }

    /// 通知正文翻页时下一屏的起始字节偏移；已经到底就回到 0。
    pub fn notify_next_page(&mut self, text: &str, offset: usize) -> usize {
        let rest = text.get(offset..).unwrap_or("");
        let avail = self.notify_text_width.max(0);
        if rest.is_empty() || self.text_width(rest) <= avail {
            return 0;
        }
        let arrow = self.text_width("›");
        let step = self.fit_prefix(rest, (avail - arrow).max(0));
        if step == 0 {
            // 连一个码点都放不下：强制前进，避免卡死。
            let advance = rest.chars().next().map(char::len_utf8).unwrap_or(1);
            let next = offset + advance;
            return if next >= text.len() { 0 } else { next };
        }
        let next = offset + step;
        if next >= text.len() { 0 } else { next }
    }

    /// 横向滚轮：向左右各挪一个码点。
    pub fn notify_scroll_chars(&self, text: &str, offset: usize, step: isize) -> usize {
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        if step > 0 {
            let mut next = offset;
            for _ in 0..step {
                match text[next..].chars().next() {
                    Some(c) => next += c.len_utf8(),
                    None => break,
                }
            }
            next.min(text.len())
        } else {
            let mut next = offset;
            for _ in 0..(-step) {
                match text[..next].chars().next_back() {
                    Some(c) => next -= c.len_utf8(),
                    None => break,
                }
            }
            next
        }
    }

    /// 一排文字的物理像素宽度（测试与状态行排版用）。
    fn text_width(&mut self, text: &str) -> i32 {
        self.painter.text_width(text)
    }

    /// 把 `text` 画到 (x, y) 起点，超出 `clip` 的像素丢弃（标题截断靠这个）。
    fn draw_text(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        x: i32,
        y: i32,
        clip: (i32, i32),
        color: Color,
    ) {
        self.painter.draw_text(pixmap, text, x, y, clip, color);
    }
}

/// 把两种颜色按 `num/den` 混合；用于通知的进度底色。
fn mix(base: [u8; 3], tint: [u8; 3], num: u32, den: u32) -> [u8; 3] {
    let den = den.max(1);
    let blend = |a: u8, b: u8| ((a as u32 * (den - num) + b as u32 * num) / den) as u8;
    [
        blend(base[0], tint[0]),
        blend(base[1], tint[1]),
        blend(base[2], tint[2]),
    ]
}

fn physical_rect(x: i32, y: i32, w: i32, h: i32, scale: f64) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((
            (x as f64 / scale).round() as i32,
            (y as f64 / scale).round() as i32,
        )),
        Size::from((
            (w as f64 / scale).round().max(1.0) as i32,
            (h as f64 / scale).round().max(1.0) as i32,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::General;

    fn view(width: i32) -> BarView {
        BarView {
            width,
            scale: 1.0,
            visible_tags: 0b101,
            occupied_tags: 0b111,
            focused_tags: 0b001,
            layout: "bsp".into(),
            title: "foot — r0wm".into(),
            status: "12:34".into(),
            notify: None,
            runner: None,
            systray: Vec::new(),
        }
    }

    fn notify_view(text: &str) -> NotifyView {
        NotifyView {
            id: 1,
            text: text.into(),
            urgency: Urgency::Normal,
            history: false,
            history_pos: None,
            queued: 0,
            left_ms: 5000,
            timeout_ms: 5000,
            actions: Vec::new(),
            pick: None,
            value: None,
            icon: None,
            offset: 0,
        }
    }

    fn bar() -> Bar {
        let mut bar = Bar::new(&General::default());
        assert!(bar.height() > 0, "默认配置下 bar 应当启用");
        assert!(bar.update(view(800)), "首次刷新必须重建位图");
        bar
    }

    #[test]
    fn bar_bitmap_matches_height() {
        let bar = bar();
        let (w, h) = bar.buffer_size;
        assert_eq!(w, 800);
        assert_eq!(h, bar.height());
        assert!(bar.buffer.is_some());
    }

    #[test]
    fn bar_draws_text_over_background() {
        let mut bar = bar();
        let (pixmap, _) = bar.rasterize(&view(800));
        let bg = [BG[2], BG[1], BG[0], 0xff];
        assert!(
            pixmap.data.chunks_exact(4).any(|px| px != bg),
            "bar 上除了底色应当还有文字或 tag 色块"
        );
    }

    #[test]
    fn bar_hits_cover_tags_layout_and_status() {
        let bar = bar();
        assert_eq!(bar.hits.len(), 11, "9 个 tag + 布局名 + 状态文本");
        assert_eq!(
            bar.hit(Point::from((PAD as f64 + 2.0, 5.0))),
            Some(BarHit::Tag(1))
        );
        assert_eq!(
            bar.hit(Point::from((790.0, 5.0))),
            Some(BarHit::Status),
            "右端应当是状态文本"
        );
        assert_eq!(bar.hit(Point::from((799.0, 999.0))), None);
    }

    #[test]
    fn status_commands_are_hit_tested_per_button() {
        let mut bar = bar();
        let mut v = view(800);
        v.status = "^s!echo hi^CLICK^e^ tail".into();
        assert!(bar.update(v));

        // 状态文本右对齐：先量出尾部 " tail" 的宽度，CLICK 就在它左边。
        let tail_w = bar.text_width(" tail") as f64;
        let right = (800 - PAD) as f64;
        let click = Point::from((right - tail_w - 1.0, 5.0));
        let tail = Point::from((right - tail_w + 1.0, 5.0));

        assert_eq!(bar.status_command(click, 1), Some("echo hi"));
        assert_eq!(bar.status_command(click, 2), None, "别的按钮没有命令");
        assert_eq!(bar.status_command(tail, 1), None, "`^e^` 之后不可点");
    }

    #[test]
    fn plain_status_has_no_commands() {
        let bar = bar();
        assert_eq!(bar.status_command(Point::from((790.0, 5.0)), 1), None);
    }

    #[test]
    fn notification_takes_over_the_center_and_is_clickable() {
        let mut bar = bar();
        let mut v = view(800);
        v.title = String::new();
        v.notify = Some(notify_view("app: hello"));
        assert!(bar.update(v));
        assert_eq!(bar.hit(Point::from((400.0, 5.0))), Some(BarHit::Notify));
        assert_eq!(bar.hit(Point::from((790.0, 5.0))), Some(BarHit::Status));
    }

    #[test]
    fn runner_draws_candidate_icons_and_missing_icon_holders() {
        let general = General {
            runner_icons: RunnerIcons::All,
            runner_pill_height: 20,
            ..General::default()
        };
        let mut bar = Bar::new(&general);
        let mut view = view(800);
        view.runner = Some(RunnerView {
            text: "fi".into(),
            cursor: 2,
            placeholder: String::new(),
            completion: None,
            calc: None,
            candidates: vec![
                RunnerCandidate {
                    label: "firefox".into(),
                    icon: Some(Arc::new(Icon {
                        width: 1,
                        height: 1,
                        rgba: vec![0xff, 0x00, 0x00, 0xff],
                    })),
                },
                RunnerCandidate {
                    label: "files".into(),
                    icon: None,
                },
            ],
            selected: 0,
            has_icons: true,
        });
        assert!(bar.update(view.clone()));
        let (pixmap, _) = bar.rasterize(&view);
        // 红色图标（B,G,R,A）。
        assert!(
            pixmap
                .data
                .chunks_exact(4)
                .any(|px| px[0] == 0x00 && px[1] == 0x00 && px[2] == 0xff && px[3] == 0xff)
        );
        // 第二条候选没有图标，画的是基线配色的占位方块。
        assert!(
            pixmap
                .data
                .chunks_exact(4)
                .any(|px| px[0] == 0x2e && px[1] == 0x2e && px[2] == 0x2e)
        );
    }

    #[test]
    fn runner_takes_over_the_center_before_notifications() {
        let mut bar = bar();
        let mut view = view(800);
        view.notify = Some(notify_view("app: hello"));
        view.runner = Some(RunnerView {
            text: "ls".into(),
            cursor: 2,
            placeholder: "Run...".into(),
            completion: Some("of".into()),
            calc: None,
            candidates: vec![
                RunnerCandidate {
                    label: "ls".into(),
                    icon: None,
                },
                RunnerCandidate {
                    label: "lsof".into(),
                    icon: None,
                },
            ],
            selected: 1,
            has_icons: false,
        });
        assert!(bar.update(view.clone()));
        // 选中候选的胶囊底色出现了；通知此时不该再画。
        let (pixmap, _) = bar.rasterize(&view);
        let pill = [SEL_BG[2], SEL_BG[1], SEL_BG[0]];
        assert!(
            pixmap
                .data
                .chunks_exact(4)
                .any(|px| px.len() == 4 && [px[0], px[1], px[2]] == pill)
        );
        assert_ne!(bar.hit(Point::from((400.0, 5.0))), Some(BarHit::Notify));
    }

    #[test]
    fn action_sign_and_chips_get_their_own_hits() {
        let mut bar = bar();
        let mut v = view(800);
        v.notify = Some(NotifyView {
            actions: vec!["Open".into(), "Reply".into()],
            ..notify_view("app: hello")
        });
        assert!(bar.update(v));
        let sign = bar
            .hits
            .iter()
            .find(|(_, hit)| *hit == BarHit::NotifySign)
            .map(|(rect, _)| {
                Point::<f64, Logical>::from((
                    rect.loc.x as f64 + rect.size.w as f64 / 2.0,
                    rect.loc.y as f64 + rect.size.h as f64 / 2.0,
                ))
            })
            .expect("有动作就必须画提示");
        assert_eq!(bar.hit(sign), Some(BarHit::NotifySign));

        let mut v = view(800);
        v.notify = Some(NotifyView {
            actions: vec!["Open".into(), "Reply".into()],
            pick: Some(1),
            ..notify_view("app: hello")
        });
        assert!(bar.update(v));
        let reply = bar
            .hits
            .iter()
            .find(|(_, hit)| *hit == BarHit::NotifyAction(1))
            .map(|(rect, _)| {
                Point::<f64, Logical>::from((
                    rect.loc.x as f64 + rect.size.w as f64 / 2.0,
                    rect.loc.y as f64 + rect.size.h as f64 / 2.0,
                ))
            })
            .expect("选择器第二个动作应当有命中区");
        assert_eq!(bar.hit(reply), Some(BarHit::NotifyAction(1)));
        assert!(
            !bar.hits.iter().any(|(_, hit)| *hit == BarHit::NotifySign),
            "选择器打开时不再画右侧提示"
        );
    }

    #[test]
    fn notification_text_scrolls_by_pages_and_chars() {
        let mut bar = bar();
        let text = format!("app: {}", "long notification body ".repeat(40));
        let mut v = view(800);
        v.notify = Some(notify_view(&text));
        assert!(bar.update(v));

        let mut offset = 0usize;
        let mut pages = 0;
        loop {
            let next = bar.notify_next_page(&text, offset);
            pages += 1;
            assert!(pages < 200, "翻页必须前进并最终回到开头");
            if next == 0 {
                break;
            }
            assert!(next > offset);
            offset = next;
        }
        assert!(pages > 1, "这段文本应当需要不止一屏");

        let step = bar.notify_scroll_chars(&text, 0, 3);
        assert!(text.is_char_boundary(step));
        assert_eq!(bar.notify_scroll_chars(&text, step, -3), 0);
        assert!(bar.notify_scroll_chars(&text, 1, -1) <= 1);
    }

    #[test]
    fn systray_icons_sit_at_the_right_end() {
        let mut bar = bar();
        let mut v = view(800);
        v.systray = vec![
            SystrayItem {
                appid: "A".into(),
                icon: None,
            },
            SystrayItem {
                appid: "B".into(),
                icon: None,
            },
        ];
        assert!(bar.update(v));

        let first = bar
            .hits
            .iter()
            .find(|(_, hit)| *hit == BarHit::Tray(0))
            .map(|(rect, _)| *rect)
            .expect("第一个托盘图标应当有命中区");
        let second = bar
            .hits
            .iter()
            .find(|(_, hit)| *hit == BarHit::Tray(1))
            .map(|(rect, _)| *rect)
            .expect("第二个托盘图标应当有命中区");
        assert!(first.loc.x < second.loc.x, "图标按注册顺序从左往右");
        assert_eq!(second.loc.x + second.size.w, bar.buffer_size.0 - 2);

        let status = bar
            .hits
            .iter()
            .find(|(_, hit)| *hit == BarHit::Status)
            .map(|(rect, _)| *rect)
            .expect("状态文本仍然存在");
        assert!(
            status.loc.x + status.size.w <= first.loc.x,
            "状态文本必须让位给托盘"
        );

        let center = Point::from((first.loc.x as f64 + first.size.w as f64 / 2.0, 5.0));
        assert_eq!(bar.hit(center), Some(BarHit::Tray(0)));
    }

    #[test]
    fn no_systray_leaves_status_at_the_edge() {
        let bar = bar();
        assert_eq!(
            bar.hit(Point::from((790.0, 5.0))),
            Some(BarHit::Status),
            "没有托盘时状态文本回到最右端"
        );
    }

    #[test]
    fn same_view_does_not_rebuild() {
        let mut bar = bar();
        assert!(!bar.update(view(800)), "内容没变就不该重建");
        assert!(bar.update(view(640)), "宽度变化必须重建");
    }

    /// 需要人眼确认时把 bar 转成 PPM：`R0WM_BAR_DUMP=/tmp/bar.ppm cargo test bar`。
    #[test]
    fn dump_bar_ppm_when_requested() {
        let Ok(path) = std::env::var("R0WM_BAR_DUMP") else {
            return;
        };
        let mut bar = bar();
        // 带上颜色与点击区转义，导出的位图能直接看出状态行的渲染效果。
        let mut v = view(800);
        v.status =
            "^c#ffcc00^10:05^d^ ^b#223344^bat 87%^d^ ^s!echo hi^vol^e^ ^c#ff5555^12%^d^".into();
        let (pixmap, _) = bar.rasterize(&v);
        let mut ppm = format!("P6\n{} {}\n255\n", pixmap.width, pixmap.height).into_bytes();
        for px in pixmap.data.chunks_exact(4) {
            ppm.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        std::fs::write(&path, ppm).expect("写 PPM 失败");
    }
}

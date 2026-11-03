//! 每窗口标题条（基线 `src/bar.c` 的 `drawtitle`）。
//!
//! 标题条是合成器自绘的位图，贴在窗口内容上方：平铺窗口从格子里扣掉一条，
//! 浮动窗口整体下移一条。同一列里的 tab 组把这一条横着平分（基线 tabbed 的
//! 「标题并排当 tab」），选中 tab 用高亮底色，其余 tab 仍可点击，点一下就切到
//! 那个窗口。关闭按钮只在指针悬停时出现（基线 `titleclose`），点击关闭窗口。

use std::collections::BTreeMap;

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

use super::{
    paint::{Painter, Pixmap},
    status::Color,
};
use crate::{config::General, shell::WindowId};

const BG: [u8; 3] = [0x1c, 0x1c, 0x22];
/// 选中 tab / 焦点窗口的底色。
const BG_SEL: [u8; 3] = [0x33, 0x44, 0x66];
const FG: [u8; 3] = [0xcc, 0xcc, 0xd2];
const FG_SEL: [u8; 3] = [0xff, 0xff, 0xff];
/// 多 tab 组里选中 tab 的左侧指示条。
const TAB_MARK: [u8; 3] = [0x8a, 0xb4, 0xf8];

/// 基线 `titleclosecolor` 的默认值。
const DEFAULT_CLOSE_COLOR: Color = [0xff, 0x5f, 0x57, 0xff];

/// 一条标题条要画的内容；整体相等才重画。
#[derive(Debug, Clone, PartialEq)]
pub struct TitleView {
    /// 逻辑宽度。
    pub width: i32,
    pub scale: f64,
    pub title: String,
    pub selected: bool,
    /// 浮动窗口在标题前画一个小圆点。
    pub floating: bool,
    /// 属于多窗口的 tab 组。
    pub tab: bool,
    /// 组内第一个（不画左边缘的选中指示条）。
    pub first_tab: bool,
    /// 指针正悬停在关闭按钮上。
    pub hover: bool,
}

/// 标题条在屏幕上的位置与 tab 归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitlePlacement {
    pub id: WindowId,
    pub rect: Rectangle<i32, Logical>,
    pub tab: bool,
    pub first_tab: bool,
}

/// 标题条的命中结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleHit {
    pub id: WindowId,
    /// 是否落在关闭按钮上。
    pub close: bool,
}

struct TitleBuffer {
    view: TitleView,
    buffer: Option<MemoryRenderBuffer>,
    /// 位图尺寸（物理像素）。
    size: (i32, i32),
    /// 关闭按钮的点击区，标题条内的逻辑坐标。
    close: Option<Rectangle<i32, Logical>>,
}

/// 所有窗口的标题条；字体与配色集中在这里。
pub struct Titles {
    painter: Painter,
    enabled: bool,
    /// 逻辑高度（禁用时 [`Titles::height`] 仍返回 0）。
    height: i32,
    padding: i32,
    center: bool,
    close_enabled: bool,
    close_color: Color,
    views: BTreeMap<WindowId, TitleBuffer>,
    placements: Vec<TitlePlacement>,
    /// 指针正悬停在其关闭按钮上的窗口。
    hovered: Option<WindowId>,
}

impl Titles {
    pub fn new(general: &General) -> Self {
        let font_size = general.font_size.max(6.0);
        let line_height = font_size * 1.25;
        let padding = general.title_padding.max(0);
        let height = (line_height.round() as i32 + padding).max(font_size as i32 + 4);
        let close_color = general
            .title_close_color
            .as_deref()
            .and_then(parse_color)
            .unwrap_or(DEFAULT_CLOSE_COLOR);

        Self {
            painter: Painter::new(font_size, line_height, general.font_family.clone()),
            enabled: general.titlebar,
            height,
            padding,
            center: general.title_center,
            close_enabled: general.title_close,
            close_color,
            views: BTreeMap::new(),
            placements: Vec::new(),
            hovered: None,
        }
    }

    /// 配置变了就重建字体与配色；下一次重排会重新填标题。
    pub fn reconfigure(&mut self, general: &General) {
        *self = Self::new(general);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 窗口内容上方要给标题条留的逻辑高度；禁用时为 0。
    pub fn height(&self) -> i32 {
        if self.enabled { self.height } else { 0 }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            self.views.clear();
            self.placements.clear();
            self.hovered = None;
        }
    }

    pub fn hovered(&self) -> Option<WindowId> {
        self.hovered
    }

    /// 本轮重排得到的标题条位置。
    pub fn set_placements(&mut self, placements: Vec<TitlePlacement>) {
        let ids: Vec<WindowId> = placements.iter().map(|slot| slot.id).collect();
        self.views.retain(|id, _| ids.contains(id));
        if self.hovered.is_some_and(|hovered| !ids.contains(&hovered)) {
            self.hovered = None;
        }
        self.placements = placements;
    }

    pub fn placements(&self) -> &[TitlePlacement] {
        &self.placements
    }

    /// 更新一条标题条；只有内容真的变了才重建位图。
    pub fn update(&mut self, id: WindowId, view: TitleView) -> bool {
        if !self.enabled || self.height <= 0 {
            return false;
        }
        if self
            .views
            .get(&id)
            .is_some_and(|entry| entry.view == view && entry.buffer.is_some())
        {
            return false;
        }
        let (pixmap, close) = self.rasterize(&view);
        let (phys_w, phys_h) = (pixmap.width, pixmap.height);
        let opaque = vec![Rectangle::from_size(Size::from((phys_w, phys_h)))];
        let buffer = MemoryRenderBuffer::from_slice(
            &pixmap.data,
            Fourcc::Argb8888,
            (phys_w, phys_h),
            1,
            Transform::Normal,
            Some(opaque),
        );
        self.views.insert(
            id,
            TitleBuffer {
                view,
                buffer: Some(buffer),
                size: (phys_w, phys_h),
                close,
            },
        );
        true
    }

    /// 指针是否落在某条标题条上；关闭按钮单独标记。
    pub fn hit(&self, pos: Point<f64, Logical>) -> Option<TitleHit> {
        // 浮动窗口的标题条叠在平铺层之上，所以从后往前找。
        self.placements.iter().rev().find_map(|slot| {
            if !slot.rect.to_f64().contains(pos) {
                return None;
            }
            let entry = self.views.get(&slot.id)?;
            let local = pos - slot.rect.loc.to_f64();
            let close = entry
                .close
                .is_some_and(|rect| rect.to_f64().contains(local));
            Some(TitleHit { id: slot.id, close })
        })
    }

    /// 更新悬停状态；返回悬停的窗口是否变化。
    pub fn set_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let next = pos.and_then(|pos| self.hit(pos)).filter(|hit| hit.close);
        let next = next.map(|hit| hit.id);
        if next == self.hovered {
            return false;
        }
        self.hovered = next;
        true
    }

    /// 把标题条作为渲染元素交出去（位置是全局逻辑坐标）。
    pub fn elements<R>(&self, renderer: &mut R) -> Vec<MemoryRenderBufferRenderElement<R>>
    where
        R: ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let mut elements = Vec::new();
        for slot in &self.placements {
            let Some(entry) = self.views.get(&slot.id) else {
                continue;
            };
            let Some(buffer) = entry.buffer.as_ref() else {
                continue;
            };
            let (phys_w, phys_h) = entry.size;
            let scale = entry.view.scale;
            let location = Point::<f64, Physical>::from((
                slot.rect.loc.x as f64 * scale,
                slot.rect.loc.y as f64 * scale,
            ));
            let src =
                Rectangle::<f64, Logical>::from_size(Size::from((phys_w as f64, phys_h as f64)));
            let size = Size::<i32, Logical>::from((slot.rect.size.w.max(1), self.height));
            let element = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                buffer,
                Some(1.0),
                Some(src),
                Some(size),
                Kind::Unspecified,
            );
            match element {
                Ok(element) => elements.push(element),
                Err(err) => tracing::warn!(id = slot.id, "标题条位图导入失败：{err}"),
            }
        }
        elements
    }

    // ---- 光栅化 ----

    fn rasterize(&mut self, view: &TitleView) -> (Pixmap, Option<Rectangle<i32, Logical>>) {
        let scale = view.scale.max(0.1);
        self.painter.set_scale(scale);
        let width = ((view.width as f64 * scale).round() as i32).max(1);
        let height = ((self.height as f64 * scale).round() as i32).max(1);
        let bg = if view.selected { BG_SEL } else { BG };
        let mut pixmap = Pixmap::new(width, height, bg);

        let pad = ((self.padding as f64 * scale).round() as i32).max(1);
        let line_h = (self.painter.line_height() as f64 * scale).round() as f32;
        let y_off = ((height as f32 - line_h) / 2.0).round() as i32;
        let fg = if view.selected {
            [FG_SEL[0], FG_SEL[1], FG_SEL[2], 0xff]
        } else {
            [FG[0], FG[1], FG[2], 0xff]
        };

        // 关闭按钮：直径取基线 `font->height * 3 / 5`，右端留半个内边距。
        let diameter =
            ((self.painter.line_height() as f64 * scale * 3.0 / 5.0).round() as i32).max(4);
        let close_x = width - pad / 2 - diameter;
        let close = if self.close_enabled && close_x > width / 2 {
            if view.hover {
                pixmap.fill_circle(
                    close_x as f64 + diameter as f64 / 2.0,
                    height as f64 / 2.0,
                    diameter as f64 / 2.0,
                    self.close_color,
                );
            }
            // 基线把按钮左侧半个内边距也算进点击区，鼠标更好按。
            Some(logical_rect(close_x - pad / 2, 0, width, height, scale))
        } else {
            None
        };

        let mut lead = pad;
        if view.floating {
            let dot = ((self.painter.line_height() as f64 * scale / 3.0).round() as i32).max(3);
            pixmap.fill_circle(
                (lead + dot / 2) as f64,
                height as f64 / 2.0,
                dot as f64 / 2.0,
                fg,
            );
            lead += dot + pad;
        }
        if view.tab && view.selected && !view.first_tab {
            let bar = (scale.round() as i32).max(1);
            pixmap.fill_rect(0, height / 4, bar, height - height / 2, TAB_MARK);
        }

        let end = match close {
            Some(rect) => ((rect.loc.x as f64 * scale).round() as i32).max(lead),
            None => width - pad,
        };
        if self.center {
            let text_w = self.painter.text_width(&view.title);
            lead = ((width - text_w) / 2).clamp(lead, (end - text_w).max(lead));
        }
        self.painter
            .draw_text(&mut pixmap, &view.title, lead, y_off, (lead, end), fg);

        (pixmap, close)
    }
}

/// 物理矩形转回逻辑矩形；`x1` 是右边界（不含）。
fn logical_rect(x: i32, y: i32, x1: i32, h: i32, scale: f64) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((
            (x as f64 / scale).round() as i32,
            (y as f64 / scale).round() as i32,
        )),
        Size::from((
            ((x1 - x) as f64 / scale).round().max(1.0) as i32,
            (h as f64 / scale).round().max(1.0) as i32,
        )),
    )
}

/// `#RRGGBB` / `#RRGGBBAA`（也接受不带 `#` 的写法）。
fn parse_color(text: &str) -> Option<Color> {
    let hex = text.trim().trim_start_matches('#');
    let parse = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    match hex.len() {
        6 => Some([parse(0..2)?, parse(2..4)?, parse(4..6)?, 0xff]),
        8 => Some([parse(0..2)?, parse(2..4)?, parse(4..6)?, parse(6..8)?]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn general() -> General {
        General::default()
    }

    fn view(width: i32, title: &str) -> TitleView {
        TitleView {
            width,
            scale: 1.0,
            title: title.into(),
            selected: true,
            floating: false,
            tab: false,
            first_tab: true,
            hover: false,
        }
    }

    fn titles() -> Titles {
        Titles::new(&general())
    }

    #[test]
    fn height_covers_a_text_line() {
        let titles = titles();
        assert_eq!(titles.height(), 21, "12px 字体：行高 15 + 内边距 6");
        let mut titles = titles;
        titles.set_enabled(false);
        assert_eq!(titles.height(), 0);
    }

    #[test]
    fn close_button_is_hit_only_on_hover_region() {
        let mut titles = titles();
        let id = 7;
        titles.set_placements(vec![TitlePlacement {
            id,
            rect: Rectangle::new(Point::from((10, 20)), Size::from((200, titles.height()))),
            tab: false,
            first_tab: true,
        }]);
        assert!(titles.update(id, view(200, "foot")));

        // 关闭按钮在右端；左侧正文区域不算关闭。
        let body = Point::from((30.0, 25.0));
        let close = Point::from((199.0, 25.0));
        assert_eq!(titles.hit(body), Some(TitleHit { id, close: false }));
        assert_eq!(titles.hit(close), Some(TitleHit { id, close: true }));
        assert_eq!(titles.hit(Point::from((500.0, 25.0))), None);

        // 悬停只在关闭按钮上才算。
        assert!(!titles.set_hover(Some(body)));
        assert!(titles.set_hover(Some(close)));
        assert_eq!(titles.hovered(), Some(id));
    }

    #[test]
    fn tab_marking_survives_width_changes() {
        let mut titles = titles();
        let id = 3;
        titles.set_placements(vec![TitlePlacement {
            id,
            rect: Rectangle::new(Point::from((0, 0)), Size::from((120, titles.height()))),
            tab: true,
            first_tab: false,
        }]);
        let mut v = view(120, "second tab");
        v.tab = true;
        v.first_tab = false;
        assert!(titles.update(id, v.clone()));
        assert!(!titles.update(id, v), "内容没变就不该重建");
        let mut wider = view(240, "second tab");
        wider.tab = true;
        wider.first_tab = false;
        assert!(titles.update(id, wider), "宽度变化必须重画");
    }

    #[test]
    fn dropped_windows_lose_their_buffers() {
        let mut titles = titles();
        titles.set_placements(vec![TitlePlacement {
            id: 1,
            rect: Rectangle::new(Point::from((0, 0)), Size::from((100, titles.height()))),
            tab: false,
            first_tab: true,
        }]);
        assert!(titles.update(1, view(100, "gone")));
        titles.set_placements(Vec::new());
        assert_eq!(titles.hit(Point::from((10.0, 5.0))), None);
    }

    #[test]
    fn close_color_accepts_hex_and_falls_back() {
        assert_eq!(parse_color("#ff5f57"), Some([0xff, 0x5f, 0x57, 0xff]));
        assert_eq!(parse_color("ff5f5780"), Some([0xff, 0x5f, 0x57, 0x80]));
        assert_eq!(parse_color("nope"), None);
    }

    /// 需要人眼确认时把两条标题条转成 PPM：
    /// `R0WM_TITLE_DUMP=/tmp/title.ppm cargo test title`。
    #[test]
    fn dump_title_ppm_when_requested() {
        let Ok(path) = std::env::var("R0WM_TITLE_DUMP") else {
            return;
        };
        let mut titles = titles();
        let mut floating = view(240, "foot — r0wm");
        floating.floating = true;
        let (selected, close) = titles.rasterize(&floating);
        let mut other = view(240, "second tab");
        other.selected = false;
        other.tab = true;
        other.first_tab = false;
        other.hover = true;
        let (tab, tab_close) = titles.rasterize(&other);
        assert!(close.is_some() && tab_close.is_some(), "两张都该有按钮");

        // 上下拼成一张，方便一起看。
        let (width, height) = (selected.width, selected.height);
        let mut ppm = format!("P6\n{} {}\n255\n", width, height * 2).into_bytes();
        for pixmap in [&selected, &tab] {
            for px in pixmap.data.chunks_exact(4) {
                ppm.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        std::fs::write(&path, ppm).expect("写 PPM 失败");
    }
}

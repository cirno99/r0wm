//! 托盘右键菜单：叠在窗口之上、跟随光标的弹出框（基线 `src/traypopup.c`）。
//!
//! 与 bar 一样是「内存位图 + 渲染元素」，不是 layer-shell 客户端。菜单内容
//! 由 [`crate::systray`] 的服务线程经 dbusmenu 拉取，这里只负责排版、高亮与
//! 命中测试。一次只显示一层；选中子菜单时服务线程会推来新的一层。

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

use super::paint::{Painter, Pixmap};
use crate::config::General;

const BG: [u8; 3] = [0x1c, 0x1c, 0x22];
const FG: [u8; 3] = [0xcc, 0xcc, 0xd2];
const SEL_BG: [u8; 3] = [0x33, 0x44, 0x66];
const BORDER: [u8; 3] = [0x50, 0x50, 0x58];
/// 文字左右留白（逻辑像素）。
const PAD: i32 = 8;

/// 弹出菜单的状态与位图。
pub struct TrayPopup {
    painter: Painter,
    labels: Vec<String>,
    /// 当前悬停的行；`None` 表示没有。
    sel: Option<usize>,
    /// 逻辑矩形（输出内坐标）。
    rect: Rectangle<i32, Logical>,
    /// 逻辑行高（基线直接用 bar 高度）。
    row_height: i32,
    /// 边框厚度（逻辑像素，画在矩形内部）。
    border: i32,
    scale: f64,
    buffer: Option<MemoryRenderBuffer>,
    /// 位图尺寸（物理像素）。
    buffer_size: (i32, i32),
}

impl TrayPopup {
    pub fn new(general: &General) -> Self {
        let font_size = general.font_size.max(6.0);
        let line_height = font_size * 1.25;
        Self {
            painter: Painter::new(font_size, line_height, general.font_family.clone()),
            labels: Vec::new(),
            sel: None,
            rect: Rectangle::default(),
            row_height: 1,
            border: 1,
            scale: 1.0,
            buffer: None,
            buffer_size: (0, 0),
        }
    }

    /// 菜单是否正在显示。
    pub fn active(&self) -> bool {
        !self.labels.is_empty()
    }

    /// 当前显示的标签；服务线程据此判断菜单有没有变化。
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn rect(&self) -> Rectangle<i32, Logical> {
        self.rect
    }

    /// 在 `at`（全局逻辑坐标）处弹出菜单，并夹到输出范围内。
    pub fn show(
        &mut self,
        labels: Vec<String>,
        at: Point<f64, Logical>,
        output: Rectangle<i32, Logical>,
        bar_height: i32,
        scale: f64,
        border: i32,
    ) {
        if labels.is_empty() {
            self.hide();
            return;
        }
        self.labels = labels;
        self.sel = None;
        self.row_height = bar_height.max(1);
        self.border = border.max(0);
        self.scale = scale.max(0.1);
        self.painter.set_scale(self.scale);

        let pad = ((PAD as f64 * self.scale).round() as i32).max(1);
        let border_phys = ((self.border as f64) * self.scale).round() as i32;
        let width = self
            .labels
            .iter()
            .map(|label| self.painter.text_width(label))
            .max()
            .unwrap_or(0)
            + pad
            + 2 * border_phys;
        let width = width
            .min(((output.size.w as f64) * self.scale).round() as i32)
            .max(1);
        let height = ((self.row_height as f64 * self.labels.len() as f64
            + 2.0 * self.border as f64)
            * self.scale)
            .round()
            .max(1.0) as i32;
        let height = height.min(((output.size.h as f64) * self.scale).round() as i32);

        // 逻辑尺寸：位图按物理像素画，元素再缩回逻辑矩形。
        let logical_w = (width as f64 / self.scale).round().max(1.0) as i32;
        let logical_h = (height as f64 / self.scale).round().max(1.0) as i32;
        let x = (at.x.round() as i32).clamp(
            output.loc.x,
            (output.loc.x + output.size.w - logical_w).max(output.loc.x),
        );
        let y = (at.y.round() as i32).clamp(
            output.loc.y,
            (output.loc.y + output.size.h - logical_h).max(output.loc.y),
        );
        self.rect = Rectangle::new(Point::from((x, y)), Size::from((logical_w, logical_h)));
        self.rebuild();
    }

    /// 收起菜单（不发送任何 D-Bus 操作）。
    pub fn hide(&mut self) {
        self.labels.clear();
        self.sel = None;
        self.buffer = None;
        self.buffer_size = (0, 0);
    }

    /// 指针移动：更新悬停行，返回是否重画。
    pub fn motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let row = self.row_at(pos);
        if row == self.sel {
            return false;
        }
        self.sel = row;
        self.rebuild();
        true
    }

    /// 指针落在哪一行；在框外返回 `None`。
    pub fn row_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        if !self.active() {
            return None;
        }
        let rect = self.rect.to_f64();
        if !rect.contains(pos) {
            return None;
        }
        let rows = self.rect.size.h - 2 * self.border;
        if rows <= 0 {
            return Some(0);
        }
        let relative = (pos.y - rect.loc.y - self.border as f64) / rows as f64;
        let row = (relative * self.labels.len() as f64) as i32;
        Some(row.clamp(0, self.labels.len() as i32 - 1) as usize)
    }

    /// 把菜单作为渲染元素交出去。
    pub fn elements<R>(&self, renderer: &mut R) -> Vec<MemoryRenderBufferRenderElement<R>>
    where
        R: ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let Some(buffer) = self.buffer.as_ref() else {
            return Vec::new();
        };
        let (phys_w, phys_h) = self.buffer_size;
        let location = Point::<f64, Physical>::from((
            self.rect.loc.x as f64 * self.scale,
            self.rect.loc.y as f64 * self.scale,
        ));
        let src = Rectangle::<f64, Logical>::from_size(Size::from((phys_w as f64, phys_h as f64)));
        let size = Size::<i32, Logical>::from((self.rect.size.w, self.rect.size.h));
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
                tracing::warn!("托盘菜单位图导入失败：{err}");
                Vec::new()
            }
        }
    }

    fn rebuild(&mut self) {
        if self.labels.is_empty() {
            self.buffer = None;
            return;
        }
        let pixmap = self.rasterize();
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
    }

    fn rasterize(&mut self) -> Pixmap {
        let scale = self.scale;
        let phys_w = ((self.rect.size.w as f64 * scale).round() as i32).max(1);
        let phys_h = ((self.rect.size.h as f64 * scale).round() as i32).max(1);
        let border = ((self.border as f64 * scale).round() as i32)
            .min((phys_w - 1).max(0))
            .min((phys_h - 1).max(0))
            .max(0);
        let rows_height = (phys_h - 2 * border).max(1);
        let row_height = (rows_height as f64 / self.labels.len() as f64)
            .round()
            .max(1.0) as i32;
        let line_height = (self.painter.line_height() as f64 * scale).round() as i32;
        let pad = ((PAD as f64 * scale).round() as i32).max(1) / 2;

        let mut pixmap = Pixmap::new(phys_w, phys_h, BORDER);
        for (index, label) in self.labels.iter().enumerate() {
            let top = border + index as i32 * row_height;
            let height = if index + 1 == self.labels.len() {
                // 最后一行吃掉除不尽的余数。
                (phys_h - border - top).max(1)
            } else {
                row_height
            };
            let background = if self.sel == Some(index) { SEL_BG } else { BG };
            pixmap.fill_rect(border, top, phys_w - 2 * border, height, background);
            let y = top + ((height - line_height) / 2).max(0);
            let fg = [FG[0], FG[1], FG[2], 0xff];
            self.painter.draw_text(
                &mut pixmap,
                label,
                border + pad,
                y,
                (border + pad, phys_w - border - pad),
                fg,
            );
        }
        pixmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::General;

    fn popup() -> TrayPopup {
        TrayPopup::new(&General::default())
    }

    fn output() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((0, 0)), Size::from((800, 600)))
    }

    #[test]
    fn show_places_rows_and_clamps_inside_the_output() {
        let mut popup = popup();
        popup.show(
            vec!["  Open".into(), "  Quit".into()],
            Point::from((100.0, 100.0)),
            output(),
            20,
            1.0,
            0,
        );
        assert!(popup.active());
        assert_eq!(popup.labels().len(), 2);
        let rect = popup.rect();
        assert!(rect.size.w > 0 && rect.size.h > 0);
        assert!(rect.loc.x >= 0 && rect.loc.y >= 0);
        assert!(rect.loc.x + rect.size.w <= 800);
        assert!(rect.loc.y + rect.size.h <= 600);
    }

    #[test]
    fn show_near_the_edge_is_pushed_back_inside() {
        let mut popup = popup();
        popup.show(
            vec!["  A".into()],
            Point::from((799.0, 599.0)),
            output(),
            20,
            1.0,
            0,
        );
        let rect = popup.rect();
        assert!(rect.loc.x + rect.size.w <= 800);
        assert!(rect.loc.y + rect.size.h <= 600);
    }

    #[test]
    fn rows_are_hit_tested_and_hover_tracks() {
        let mut popup = popup();
        popup.show(
            vec!["  A".into(), "  B".into(), "  C".into()],
            Point::from((50.0, 50.0)),
            output(),
            20,
            1.0,
            0,
        );
        let rect = popup.rect();
        let row0 = Point::from((rect.loc.x as f64 + 4.0, rect.loc.y as f64 + 1.0));
        let row2 = Point::from((
            rect.loc.x as f64 + 4.0,
            (rect.loc.y + rect.size.h - 2) as f64,
        ));
        assert_eq!(popup.row_at(row0), Some(0));
        assert_eq!(popup.row_at(row2), Some(2));
        assert_eq!(popup.row_at(Point::from((-10.0, -10.0))), None);
        assert!(popup.motion(row2));
        assert!(!popup.motion(row2), "同一行不重复重画");
        popup.hide();
        assert!(!popup.active());
        assert_eq!(popup.row_at(row0), None);
    }

    #[test]
    fn empty_labels_do_not_show_a_menu() {
        let mut popup = popup();
        popup.show(Vec::new(), Point::from((10.0, 10.0)), output(), 20, 1.0, 0);
        assert!(!popup.active());
    }

    #[test]
    fn rasterized_popup_draws_a_selected_row() {
        let mut popup = popup();
        popup.show(
            vec!["  A".into(), "  B".into()],
            Point::from((10.0, 10.0)),
            output(),
            20,
            1.0,
            0,
        );
        popup.motion(Point::from((15.0, 12.0)));
        let pixmap = popup.rasterize();
        let selected = [SEL_BG[2], SEL_BG[1], SEL_BG[0]];
        assert!(
            pixmap
                .data
                .chunks_exact(4)
                .any(|px| [px[0], px[1], px[2]] == selected),
            "悬停行应当有选中底色"
        );
    }
}

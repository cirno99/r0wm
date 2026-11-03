//! bar 与标题条共用的文字光栅化 / 内存位图底座。
//!
//! 合成器自绘的界面都是「cosmic-text 光栅化到 ARGB 内存位图，再交给渲染器
//! 当纹理导入」这一条路。字体、行高、缩放的处理集中在这里，bar 与标题条
//! 只负责排版与配色，避免两份字体栈各自漂移。

use cosmic_text::{
    Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};

use super::status::Color;

/// 一套字体设置 + 光栅化缓存。
pub struct Painter {
    font_size: f32,
    line_height: f32,
    family: Option<String>,
    scale: f64,
    font_system: FontSystem,
    cache: SwashCache,
}

impl Painter {
    pub fn new(font_size: f32, line_height: f32, family: Option<String>) -> Self {
        Self {
            font_size,
            line_height,
            family,
            scale: 1.0,
            font_system: FontSystem::new(),
            cache: SwashCache::new(),
        }
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// 当前输出缩放（命中测试要把逻辑坐标换成位图物理像素）。
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// 当前输出缩放；文字按物理像素光栅化，位图坐标也是物理像素。
    pub fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
    }

    /// 一排文字的物理像素宽度。
    pub fn text_width(&mut self, text: &str) -> i32 {
        let buffer = self.text_buffer(text);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0f32, f32::max);
        width.ceil() as i32
    }

    fn text_buffer(&mut self, text: &str) -> Buffer {
        let px = self.font_size * self.scale as f32;
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(px, self.line_height * self.scale as f32),
        );
        buffer.set_wrap(Wrap::None);
        let attrs = match self.family.as_deref() {
            Some(name) => Attrs::new().family(Family::Name(name)),
            None => Attrs::new().family(Family::Monospace),
        };
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    /// 把 `text` 画到 (x, y) 起点，超出 `clip` 的像素丢弃（标题截断靠这个）。
    pub fn draw_text(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        x: i32,
        y: i32,
        clip: (i32, i32),
        color: Color,
    ) {
        let mut buffer = self.text_buffer(text);
        let color = TextColor::rgba(color[0], color[1], color[2], color[3]);
        buffer.draw(
            &mut self.font_system,
            &mut self.cache,
            color,
            |gx, gy, w, h, pixel| {
                pixmap.blend(gx + x, gy + y, w, h, pixel, clip);
            },
        );
    }
}

/// ARGB8888（内存字节序 B,G,R,A）的简单画布。
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

impl Pixmap {
    /// 导出成 P6 PPM（调试用；`R0WM_BAR_DUMP=/tmp/bar.ppm` 时合成器会写它）。
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.reserve((self.width * self.height * 3) as usize);
        for px in self.data.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        out
    }

    pub fn new(width: i32, height: i32, bg: [u8; 3]) -> Self {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&[bg[2], bg[1], bg[0], 0xff]);
        }
        Self {
            width,
            height,
            data,
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, rgb: [u8; 3]) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let i = ((py * self.width + px) * 4) as usize;
                self.data[i] = rgb[2];
                self.data[i + 1] = rgb[1];
                self.data[i + 2] = rgb[0];
                self.data[i + 3] = 0xff;
            }
        }
    }

    /// 半透明色块；`alpha = 255` 时等价于 `fill_rect`。
    pub fn blend_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        let [r, g, b, a] = color;
        if a == 0 {
            return;
        }
        let alpha = a as u32;
        let inv = 255 - alpha;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let i = ((py * self.width + px) * 4) as usize;
                self.data[i] = ((b as u32 * alpha + self.data[i] as u32 * inv) / 255) as u8;
                self.data[i + 1] = ((g as u32 * alpha + self.data[i + 1] as u32 * inv) / 255) as u8;
                self.data[i + 2] = ((r as u32 * alpha + self.data[i + 2] as u32 * inv) / 255) as u8;
                self.data[i + 3] = 0xff;
            }
        }
    }

    /// 抗锯齿实心圆（标题条上的浮动标记与关闭按钮）。
    pub fn fill_circle(&mut self, cx: f64, cy: f64, radius: f64, color: Color) {
        if radius <= 0.0 || color[3] == 0 {
            return;
        }
        let x0 = ((cx - radius - 1.0).floor() as i32).max(0);
        let y0 = ((cy - radius - 1.0).floor() as i32).max(0);
        let x1 = ((cx + radius + 1.0).ceil() as i32).min(self.width);
        let y1 = ((cy + radius + 1.0).ceil() as i32).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let dx = px as f64 + 0.5 - cx;
                let dy = py as f64 + 0.5 - cy;
                let coverage = (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let alpha = (color[3] as f64 * coverage).round() as u32;
                self.blend_pixel(px, py, [color[0], color[1], color[2], alpha as u8]);
            }
        }
    }

    /// 抗锯齿实心圆角矩形；`radius` 会被夹到短边的一半。
    ///
    /// 用「到圆角矩形的有符号距离」算覆盖率，和 [`Self::fill_circle`] 一样
    /// 只混合圆角附近的像素，直边区域保持整像素填充。
    pub fn fill_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: f64, color: Color) {
        if w <= 0 || h <= 0 || color[3] == 0 {
            return;
        }
        let r = radius.clamp(0.0, w.min(h) as f64 / 2.0);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        let cx = x as f64 + w as f64 / 2.0;
        let cy = y as f64 + h as f64 / 2.0;
        let hx = w as f64 / 2.0 - r;
        let hy = h as f64 / 2.0 - r;
        for py in y0..y1 {
            for px in x0..x1 {
                let qx = (px as f64 + 0.5 - cx).abs() - hx;
                let qy = (py as f64 + 0.5 - cy).abs() - hy;
                let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
                let coverage = (0.5 - dist).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let alpha = (color[3] as f64 * coverage).round() as u32;
                self.blend_pixel(px, py, [color[0], color[1], color[2], alpha as u8]);
            }
        }
    }

    /// 直通道 alpha 的 src-over 混合；背景是不透明的，所以结果也保持不透明。
    pub fn blend(&mut self, x: i32, y: i32, w: u32, h: u32, color: TextColor, clip: (i32, i32)) {
        let (r, g, b, a) = color.as_rgba_tuple();
        if a == 0 {
            return;
        }
        let x0 = x.max(clip.0).max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(clip.1).min(self.width);
        let y1 = (y + h as i32).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                self.blend_pixel(px, py, [r, g, b, a]);
            }
        }
    }

    /// 把 RGBA 图标缩放到 (x, y, w, h)，最近邻采样，带 alpha 混合。
    pub fn blit_rgba(&mut self, x: i32, y: i32, w: i32, h: i32, icon: &super::icon::Icon) {
        if icon.width <= 0 || icon.height <= 0 || w <= 0 || h <= 0 {
            return;
        }
        for dy in 0..h {
            let py = y + dy;
            if py < 0 || py >= self.height {
                continue;
            }
            let sy = (dy as i64 * icon.height as i64 / h as i64) as i32;
            for dx in 0..w {
                let px = x + dx;
                if px < 0 || px >= self.width {
                    continue;
                }
                let sx = (dx as i64 * icon.width as i64 / w as i64) as i32;
                let i = ((sy * icon.width + sx) * 4) as usize;
                let [r, g, b, a] = [
                    icon.rgba[i],
                    icon.rgba[i + 1],
                    icon.rgba[i + 2],
                    icon.rgba[i + 3],
                ];
                self.blend_pixel(px, py, [r, g, b, a]);
            }
        }
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: [u8; 4]) {
        let [r, g, b, a] = color;
        if a == 0 {
            return;
        }
        let alpha = a as u32;
        let inv = 255 - alpha;
        let i = ((y * self.width + x) * 4) as usize;
        self.data[i] = ((b as u32 * alpha + self.data[i] as u32 * inv) / 255) as u8;
        self.data[i + 1] = ((g as u32 * alpha + self.data[i + 1] as u32 * inv) / 255) as u8;
        self.data[i + 2] = ((r as u32 * alpha + self.data[i + 2] as u32 * inv) / 255) as u8;
        self.data[i + 3] = 0xff;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blue(pixmap: &Pixmap, x: i32, y: i32) -> u8 {
        pixmap.data[((y * pixmap.width + x) * 4) as usize]
    }

    #[test]
    fn round_rect_fills_the_middle_and_clears_the_corners() {
        let mut pixmap = Pixmap::new(10, 10, [0x00, 0x00, 0x00]);
        pixmap.fill_round_rect(0, 0, 10, 10, 5.0, [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(blue(&pixmap, 5, 5), 0xff);
        // 上边中点落在圆内，只留一点抗锯齿边。
        assert!(blue(&pixmap, 5, 0) > 200);
        // 全圆时四个角都不该有像素（抗锯齿外沿也不到 1/4 覆盖）。
        assert_eq!(blue(&pixmap, 0, 0), 0x00);
        assert_eq!(blue(&pixmap, 9, 9), 0x00);
    }

    #[test]
    fn round_rect_is_plain_when_the_radius_is_zero() {
        let mut pixmap = Pixmap::new(4, 4, [0x00, 0x00, 0x00]);
        pixmap.fill_round_rect(0, 0, 4, 4, 0.0, [0xff, 0xff, 0xff, 0xff]);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(blue(&pixmap, x, y), 0xff);
            }
        }
    }
}

//! 光标：XCursor 主题加载与渲染元素。
//!
//! 基线 g0wm 由 `wlr_cursor` 加载 `XCURSOR_THEME` / `XCURSOR_SIZE` 指定的主题；
//! r0wm 直接读同一组环境变量，按需解析主题里的图标文件，并在缺主题时退回内嵌箭头
//! （`assets/cursor.rgba`，取自 Smithay anvil，MIT）。
//!
//! 命名光标和客户端自绘光标（`wl_pointer.set_cursor`）都走这里的渲染元素。

use std::{collections::HashMap, fs};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem, Renderer, Texture,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
        },
    },
    input::pointer::CursorImageStatus,
    render_elements,
    utils::{Logical, Physical, Point, Scale, Transform},
};
use xcursor::{CursorTheme, parser::Image};

/// 主题缺失时的兜底光标：64×64 ARGB8888，热点在左上角。
const FALLBACK_CURSOR: &[u8] = include_bytes!("../../assets/cursor.rgba");
const FALLBACK_SIZE: u32 = 32;

render_elements! {
    /// 光标渲染元素：客户端自绘表面，或主题里的内存位图。
    pub CursorRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for CursorRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Memory(arg0) => f.debug_tuple("Memory").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

/// 指针当前该画什么：命名光标缓存位图，自绘光标直接用 surface。
pub struct PointerElement {
    status: CursorImageStatus,
    /// 命名光标的位图与热点（逻辑坐标，热点已在加载时除掉缩放）。
    named: Option<(MemoryRenderBuffer, Point<i32, Logical>)>,
    theme: Option<CursorTheme>,
    /// `XCURSOR_SIZE` 的标称尺寸（逻辑像素）。
    size: u32,
    /// 图标名@缩放 → (位图, 热点)；避免每次换形状都重新读文件。
    cache: HashMap<String, (MemoryRenderBuffer, Point<i32, Logical>)>,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self::new()
    }
}

impl PointerElement {
    pub fn new() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(24);
        let mut element = Self {
            status: CursorImageStatus::default_named(),
            named: None,
            theme: Some(CursorTheme::load(&name)),
            size,
            cache: HashMap::new(),
        };
        element.set_status(CursorImageStatus::default_named(), 1);
        element
    }

    /// 换光标形状；命名光标在这里就把位图准备好。
    pub fn set_status(&mut self, status: CursorImageStatus, scale: u32) {
        if let CursorImageStatus::Named(icon) = &status {
            let scale = scale.max(1);
            let key = format!("{}@{scale}", icon.name());
            if !self.cache.contains_key(&key) {
                if let Some(entry) = self.load_icon(icon.name(), scale) {
                    self.cache.insert(key.clone(), entry);
                }
            }
            self.named = self.cache.get(&key).cloned();
        }
        self.status = status;
    }

    /// 当前光标热点（逻辑坐标）。自绘光标的热点来自 surface 附加数据。
    pub fn hotspot(&self) -> Point<i32, Logical> {
        match &self.status {
            CursorImageStatus::Surface(surface) => {
                smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<smithay::input::pointer::CursorImageSurfaceData>()
                        .map(|data| data.lock().unwrap().hotspot)
                        .unwrap_or_default()
                })
            }
            _ => self
                .named
                .as_ref()
                .map(|(_, hotspot)| *hotspot)
                .unwrap_or_default(),
        }
    }

    fn load_icon(
        &self,
        name: &str,
        scale: u32,
    ) -> Option<(MemoryRenderBuffer, Point<i32, Logical>)> {
        match self.load_themed(name, scale) {
            Some(entry) => Some(entry),
            None => Some(fallback(scale)),
        }
    }

    fn load_themed(
        &self,
        name: &str,
        scale: u32,
    ) -> Option<(MemoryRenderBuffer, Point<i32, Logical>)> {
        let theme = self.theme.as_ref()?;
        let path = theme
            .load_icon(name)
            .or_else(|| theme.load_icon("default"))?;
        let data = fs::read(path).ok()?;
        let images = xcursor::parser::parse_xcursor(&data)?;
        let image = choose_frame(0, self.size * scale, &images)?;

        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            scale as i32,
            Transform::Normal,
            None,
        );
        Some((buffer, scaled_hotspot(image, scale)))
    }

    /// 把当前光标转成渲染元素；`location` 是左上角（已扣掉热点）。
    pub fn render_elements<T, R, E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        T: Texture + Clone + Send + 'static,
        R: Renderer<TextureId = T> + ImportAll + ImportMem,
        E: From<CursorRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => Vec::new(),
            // 命名光标的形状在 `set_status` 里已选好，这里只画缓存位图。
            CursorImageStatus::Named(_) => {
                let Some((buffer, _)) = self.named.as_ref() else {
                    return Vec::new();
                };
                let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) else {
                    return Vec::new();
                };
                vec![E::from(CursorRenderElement::Memory(element))]
            }
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<CursorRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        alpha,
                        Kind::Cursor,
                    );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}

/// 按标称尺寸挑最接近的一组帧，再按时间选当前帧。
fn choose_frame(millis: u32, size: u32, images: &[Image]) -> Option<&Image> {
    let frames: Vec<&Image> = nearest_images(size, images).collect();
    if frames.is_empty() {
        return None;
    }

    let total: u32 = frames.iter().map(|image| image.delay).sum();
    if total == 0 {
        return frames.first().copied();
    }

    let mut left = millis % total;
    for image in frames {
        if left < image.delay {
            return Some(image);
        }
        left -= image.delay;
    }
    None
}

/// 标称尺寸最接近的那一组帧（同一动画的不同帧宽高一致）。
fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    let nearest = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs());
    let dims = nearest.map(|image| (image.width, image.height));

    images
        .iter()
        .filter(move |image| Some((image.width, image.height)) == dims)
}

/// 位图按 `scale` 放大，热点要缩回逻辑坐标。
fn scaled_hotspot(image: &Image, scale: u32) -> Point<i32, Logical> {
    let scale = scale.max(1) as i32;
    Point::from((image.xhot as i32 / scale, image.yhot as i32 / scale))
}

/// 内嵌兜底箭头。
fn fallback(scale: u32) -> (MemoryRenderBuffer, Point<i32, Logical>) {
    let side = FALLBACK_SIZE * 2;
    let buffer = MemoryRenderBuffer::from_slice(
        FALLBACK_CURSOR,
        Fourcc::Argb8888,
        (side as i32, side as i32),
        scale as i32,
        Transform::Normal,
        None,
    );
    (buffer, Point::from((1, 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(size: u32, delay: u32, width: u32, height: u32) -> Image {
        Image {
            size,
            width,
            height,
            xhot: 2,
            yhot: 3,
            delay,
            pixels_rgba: vec![0; (width * height * 4) as usize],
            pixels_argb: Vec::new(),
        }
    }

    #[test]
    fn picks_the_nearest_nominal_size() {
        let images = vec![image(24, 1, 32, 32), image(48, 1, 64, 64)];
        assert_eq!(nearest_images(40, &images).count(), 1);
        assert_eq!(choose_frame(0, 40, &images).unwrap().width, 64);
    }

    #[test]
    fn animates_by_frame_delay() {
        let images = vec![image(24, 10, 16, 16), image(24, 20, 16, 16)];
        assert_eq!(choose_frame(0, 24, &images).unwrap().delay, 10);
        assert_eq!(choose_frame(10, 24, &images).unwrap().delay, 20);
        // 30ms 一轮后回到第一帧。
        assert_eq!(choose_frame(30, 24, &images).unwrap().delay, 10);
    }

    #[test]
    fn hotspot_is_logical() {
        let frame = image(24, 1, 64, 64);
        assert_eq!(scaled_hotspot(&frame, 1), Point::from((2, 3)));
        assert_eq!(scaled_hotspot(&frame, 2), Point::from((1, 1)));
    }
}

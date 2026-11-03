//! 图标查找与解码（通知、runner、systray 共用）。
//!
//! 查找顺序对齐基线 `src/systray/icon.c` 的 `iconfile()`：**只查 `hicolor`
//! 主题**，不解析 `index.theme`、不做主题继承；尺寸探测顺序也照抄基线的
//! `iconsizes[]`。能解码的是 PNG 与 SVG（基线靠 gdk-pixbuf，还支持 `.xpm`；
//! 那个格式这里跳过，继续找下一个候选）。
//!
//! 缩放语义同样照抄基线：
//! - 服务端下发的 `image-data` 只缩不放（`shrink`）；
//! - 从文件读到的图标缩放到方框内，允许放大（`fit`）。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// 解码后的 RGBA 图标（未预乘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub width: i32,
    pub height: i32,
    pub rgba: Vec<u8>,
}

/// 基线 `iconsizes[]`：探测 `hicolor/<N>x<N>/apps` 的先后顺序。
const ICON_SIZES: [u32; 8] = [24, 32, 22, 48, 64, 16, 128, 256];
/// 真能解码的扩展名（基线还认 `.xpm`）。
const EXTENSIONS: [&str; 2] = ["png", "svg"];
/// 缓存条目上限；满了直接清空重建，图标本来就不多。
const CACHE_MAX: usize = 64;

/// `(名字, 主题根, 目标边长)`。
type CacheKey = (String, Option<PathBuf>, u32);

static CACHE: OnceLock<Mutex<HashMap<CacheKey, Option<Icon>>>> = OnceLock::new();

/// 按名字（或绝对路径 / `file://` URI）找图标并解码，结果带缓存。
///
/// 失败也会被缓存：图标主题目录一般不会在运行时变化，这样可以避免每个通知都
/// 把磁盘扫一遍。
pub fn load_named(name: &str, theme_path: Option<&Path>, size: u32) -> Option<Icon> {
    if name.is_empty() {
        return None;
    }
    let key = (name.to_string(), theme_path.map(Path::to_path_buf), size);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(&key)
    {
        return hit.clone();
    }

    let icon = find(name, theme_path).and_then(|path| load_path(&path, size));
    if let Ok(mut map) = cache.lock() {
        if map.len() >= CACHE_MAX {
            map.clear();
        }
        map.insert(key, icon.clone());
    }
    icon
}

/// 找到 `name` 对应的文件路径。
pub fn find(name: &str, theme_path: Option<&Path>) -> Option<PathBuf> {
    let name = name.strip_prefix("file://").unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    // 客户端有时把路径塞在 IconName 里。
    if name.starts_with('/') {
        return readable(Path::new(name));
    }
    // 名字里出现 `/` 只会是越界路径，直接拒绝。
    if name.contains('/') {
        return None;
    }
    find_in(name, theme_path, &data_home(), &data_dirs())
}

/// 解码一个图标文件；`size == 0` 表示保持原尺寸。
pub fn load_path(path: &Path, size: u32) -> Option<Icon> {
    let data = std::fs::read(path).ok()?;
    if path.extension().is_some_and(|ext| ext == "svg") {
        return render_svg(&data, path, size);
    }
    let image = image::load_from_memory(&data).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(fit(image.as_raw(), width, height, size))
}

/// 缩放到 `max × max` 的方框内（保持比例，允许放大）。
pub fn fit(rgba: &[u8], width: u32, height: u32, max: u32) -> Icon {
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        return empty();
    }
    let (dst_w, dst_h) = if max == 0 {
        (width, height)
    } else {
        boxed_size(width, height, max, true)
    };
    resize(rgba, width, height, dst_w, dst_h)
}

/// 只缩不放（`image-data` 的语义）。
pub fn shrink(rgba: &[u8], width: u32, height: u32, max: u32) -> Icon {
    if width <= max && height <= max {
        return Icon {
            width: width as i32,
            height: height as i32,
            rgba: rgba.to_vec(),
        };
    }
    fit(rgba, width, height, max)
}

/// 空的（零尺寸）图标：调用方据此跳过绘制。
pub fn empty() -> Icon {
    Icon {
        width: 0,
        height: 0,
        rgba: Vec::new(),
    }
}

/// 等比例缩进方框的尺寸；`allow_up` 为假时不放大。
fn boxed_size(width: u32, height: u32, max: u32, allow_up: bool) -> (u32, u32) {
    if !allow_up && width <= max && height <= max {
        return (width, height);
    }
    if width >= height {
        (max, (height * max / width).max(1))
    } else {
        ((width * max / height).max(1), max)
    }
}

fn data_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
}

fn data_dirs() -> Vec<PathBuf> {
    match std::env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => std::env::split_paths(&dirs).collect(),
        None => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    }
}

/// 查找主体，目录列表由调用方给出（便于单测，不碰进程环境）。
fn find_in(
    name: &str,
    theme_path: Option<&Path>,
    data_home: &Path,
    data_dirs: &[PathBuf],
) -> Option<PathBuf> {
    // 基线：Systray 的 IconThemePath 优先，形态是「主题根 + 平铺文件」。
    if let Some(theme) = theme_path
        && let Some(path) = find_themed(theme, name).or_else(|| find_flat(theme, name))
    {
        return Some(path);
    }
    for base in std::iter::once(data_home).chain(data_dirs.iter().map(PathBuf::as_path)) {
        if let Some(path) = find_themed(&base.join("icons"), name)
            .or_else(|| find_flat(&base.join("pixmaps"), name))
        {
            return Some(path);
        }
    }
    None
}

/// `hicolor/<N>x<N>/apps/<name>.png`，再退到 `hicolor/scalable/apps/<name>.svg`。
fn find_themed(root: &Path, name: &str) -> Option<PathBuf> {
    let hicolor = root.join("hicolor");
    for size in ICON_SIZES {
        let path = hicolor
            .join(format!("{size}x{size}"))
            .join("apps")
            .join(format!("{name}.png"));
        if is_file(&path) {
            return Some(path);
        }
    }
    let path = hicolor
        .join("scalable")
        .join("apps")
        .join(format!("{name}.svg"));
    is_file(&path).then_some(path)
}

/// 平铺目录里的 `<name>.png` / `<name>.svg`。
fn find_flat(root: &Path, name: &str) -> Option<PathBuf> {
    EXTENSIONS.iter().find_map(|ext| {
        let path = root.join(format!("{name}.{ext}"));
        is_file(&path).then_some(path)
    })
}

fn is_file(path: &Path) -> bool {
    path.is_file()
}

fn readable(path: &Path) -> Option<PathBuf> {
    is_file(path).then(|| path.to_path_buf())
}

/// 双线性重采样（预乘空间做，避免透明边缘发黑）。
fn resize(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Icon {
    if (src_w, src_h) == (dst_w, dst_h) {
        return Icon {
            width: dst_w as i32,
            height: dst_h as i32,
            rgba: src.to_vec(),
        };
    }
    let mut rgba = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for y in 0..dst_h {
        let sy =
            ((y as f32 + 0.5) * src_h as f32 / dst_h as f32 - 0.5).clamp(0.0, src_h as f32 - 1.0);
        let y0 = sy.floor() as u32;
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = sy - y0 as f32;
        for x in 0..dst_w {
            let sx = ((x as f32 + 0.5) * src_w as f32 / dst_w as f32 - 0.5)
                .clamp(0.0, src_w as f32 - 1.0);
            let x0 = sx.floor() as u32;
            let x1 = (x0 + 1).min(src_w - 1);
            let fx = sx - x0 as f32;

            let mut acc = [0.0f32; 4];
            for (px, py, weight) in [
                (x0, y0, (1.0 - fx) * (1.0 - fy)),
                (x1, y0, fx * (1.0 - fy)),
                (x0, y1, (1.0 - fx) * fy),
                (x1, y1, fx * fy),
            ] {
                if weight == 0.0 {
                    continue;
                }
                let i = ((py * src_w + px) * 4) as usize;
                let alpha = src[i + 3] as f32;
                acc[0] += weight * src[i] as f32 * alpha / 255.0;
                acc[1] += weight * src[i + 1] as f32 * alpha / 255.0;
                acc[2] += weight * src[i + 2] as f32 * alpha / 255.0;
                acc[3] += weight * alpha;
            }

            let alpha = acc[3];
            let o = ((y * dst_w + x) * 4) as usize;
            if alpha > 0.5 {
                rgba[o] = (acc[0] * 255.0 / alpha).clamp(0.0, 255.0) as u8;
                rgba[o + 1] = (acc[1] * 255.0 / alpha).clamp(0.0, 255.0) as u8;
                rgba[o + 2] = (acc[2] * 255.0 / alpha).clamp(0.0, 255.0) as u8;
                rgba[o + 3] = alpha.clamp(0.0, 255.0) as u8;
            }
        }
    }
    Icon {
        width: dst_w as i32,
        height: dst_h as i32,
        rgba,
    }
}

/// 把 SVG 直接光栅化到目标尺寸（比先按固有尺寸画再放大清楚）。
fn render_svg(data: &[u8], path: &Path, max: u32) -> Option<Icon> {
    let options = resvg::usvg::Options {
        resources_dir: path.parent().map(Path::to_path_buf),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(data, &options).ok()?;
    let size = tree.size();
    let (src_w, src_h) = (size.width(), size.height());
    if !(src_w > 0.0 && src_h > 0.0) {
        return None;
    }
    let (dst_w, dst_h) = if max == 0 {
        (src_w.round().max(1.0) as u32, src_h.round().max(1.0) as u32)
    } else if src_w >= src_h {
        (max, ((src_h * max as f32 / src_w).round() as u32).max(1))
    } else {
        (((src_w * max as f32 / src_h).round() as u32).max(1), max)
    };
    let mut pixmap = resvg::tiny_skia::Pixmap::new(dst_w, dst_h)?;
    let transform =
        resvg::tiny_skia::Transform::from_scale(dst_w as f32 / src_w, dst_h as f32 / src_h);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut rgba = pixmap.data().to_vec();
    unpremultiply(&mut rgba);
    Some(Icon {
        width: dst_w as i32,
        height: dst_h as i32,
        rgba,
    })
}

/// tiny-skia 输出预乘 alpha，bar 的混合按直通 alpha 算。
fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// 每个测试一个独立目录，避免并行测试互相踩。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "r0wm-icon-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn sizes_probed_in_baseline_order() {
        let base = temp_dir("order");
        touch(&base.join("icons/hicolor/24x24/apps/app.png"));
        touch(&base.join("icons/hicolor/48x48/apps/app.png"));
        let path = find_in("app", None, &base, &[]).unwrap();
        assert!(path.ends_with("24x24/apps/app.png"), "{path:?}");
    }

    #[test]
    fn scalable_svg_after_raster_sizes() {
        let base = temp_dir("scalable");
        touch(&base.join("icons/hicolor/scalable/apps/app.svg"));
        touch(&base.join("icons/hicolor/128x128/apps/app.png"));
        let path = find_in("app", None, &base, &[]).unwrap();
        assert!(path.ends_with("128x128/apps/app.png"), "{path:?}");
        std::fs::remove_file(base.join("icons/hicolor/128x128/apps/app.png")).unwrap();
        let path = find_in("app", None, &base, &[]).unwrap();
        assert!(path.ends_with("scalable/apps/app.svg"), "{path:?}");
    }

    #[test]
    fn falls_back_to_pixmaps_in_later_data_dir() {
        let home = temp_dir("home");
        let first = temp_dir("first");
        let second = temp_dir("second");
        touch(&second.join("pixmaps/thing.png"));
        let dirs = vec![first, second.clone()];
        let path = find_in("thing", None, &home, &dirs).unwrap();
        assert_eq!(path, second.join("pixmaps/thing.png"));
    }

    #[test]
    fn theme_path_wins_over_data_dirs() {
        let home = temp_dir("home2");
        let theme = temp_dir("theme");
        touch(&theme.join("hicolor/32x32/apps/item.png"));
        touch(&home.join("icons/hicolor/24x24/apps/item.png"));
        let path = find_in("item", Some(&theme), &home, &[]).unwrap();
        assert!(path.starts_with(&theme), "{path:?}");
        assert!(path.ends_with("32x32/apps/item.png"), "{path:?}");
    }

    #[test]
    fn absolute_path_and_file_uri_are_accepted() {
        let dir = temp_dir("abs");
        let file = dir.join("icon.png");
        touch(&file);
        let uri = format!("file://{}", file.display());
        assert_eq!(find(&uri, None).unwrap(), file);
        assert_eq!(find(file.to_str().unwrap(), None).unwrap(), file);
    }

    #[test]
    fn path_separators_in_names_are_rejected() {
        assert!(find("../../etc/passwd", None).is_none());
        assert!(find("", None).is_none());
    }

    #[test]
    fn fit_keeps_aspect_and_shrinks_only_when_asked() {
        let rgba = vec![0xff; 100 * 50 * 4];
        let icon = fit(&rgba, 100, 50, 64);
        assert_eq!((icon.width, icon.height), (64, 32));
        let icon = shrink(&rgba, 40, 20, 64);
        assert_eq!((icon.width, icon.height), (40, 20));
    }

    #[test]
    fn resize_keeps_alpha_and_color() {
        // 2x1：全红不透明 + 全透明。
        let src = [255, 0, 0, 255, 0, 0, 0, 0];
        let icon = fit(&src, 2, 1, 1);
        assert_eq!((icon.width, icon.height), (1, 1));
        assert!(icon.rgba[3] >= 127, "半透明像素应保留约一半 alpha");
        assert_eq!(icon.rgba[0], 255, "红色通道不能被透明像素拉黑");
    }

    #[test]
    fn decodes_png_from_disk() {
        let dir = temp_dir("png");
        let file = dir.join("icon.png");
        let image = image::RgbaImage::from_pixel(8, 4, image::Rgba([1, 2, 3, 255]));
        image.save(&file).unwrap();
        let icon = load_path(&file, 64).unwrap();
        // 文件图标允许放大：8x4 -> 64x32。
        assert_eq!((icon.width, icon.height), (64, 32));
        assert_eq!(&icon.rgba[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn renders_svg_from_disk() {
        let dir = temp_dir("svg");
        let file = dir.join("icon.svg");
        std::fs::write(
            &file,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
                 <rect width="20" height="10" fill="#ff0000"/>
               </svg>"##,
        )
        .unwrap();
        let icon = load_path(&file, 64).unwrap();
        assert_eq!((icon.width, icon.height), (64, 32));
        let center = ((16 * 64) + 32) * 4;
        assert_eq!(&icon.rgba[center..center + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn load_named_caches_hits_and_misses() {
        let dir = temp_dir("cache");
        let file = dir.join("hicolor/64x64/apps/cached.png");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 8, 7, 255]))
            .save(&file)
            .unwrap();

        let icon = load_named("cached", Some(&dir), 64).unwrap();
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(&icon.rgba[..4], &[9, 8, 7, 255]);

        // 文件删掉后第二次仍命中缓存。
        std::fs::remove_file(&file).unwrap();
        assert!(load_named("cached", Some(&dir), 64).is_some());
        // miss 也会被缓存。
        assert!(load_named("nope", Some(&dir), 64).is_none());
        assert!(load_named("nope", Some(&dir), 64).is_none());
    }

    #[test]
    fn missing_file_is_none() {
        assert!(load_path(Path::new("/nonexistent/r0wm/icon.png"), 64).is_none());
    }
}

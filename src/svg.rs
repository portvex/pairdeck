//! SVG rasterization (resvg, pure Rust) for the preview pane and the viewer.

use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use resvg::{tiny_skia, usvg};
use std::sync::{Arc, OnceLock};

/// Longest side of the rendered bitmap. Vectors are rendered this large so that
/// zooming in the preview still has real detail.
pub const MAX_SIDE: u32 = 2048;

/// Refuse absurd inputs before parsing (the preview already caps reads at 64 MiB).
const MAX_SVG_BYTES: usize = 32 << 20;

/// System fonts, loaded once (used for `<text>` elements).
fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

/// Load the font database on a background thread so the first SVG renders fast.
pub fn warm_up() {
    std::thread::Builder::new().name("fontdb".into()).spawn(|| drop(fontdb())).ok();
}

pub fn is_svg_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".svg") || n.ends_with(".svgz")
}

/// A rendered SVG and its intrinsic size in user units.
pub struct Rendered {
    pub image: DynamicImage,
    pub width: f32,
    pub height: f32,
}

/// Parse SVG or SVGZ bytes and render them so the longest side is `max_side`
/// pixels, composited over a light checkerboard (most SVGs are transparent with
/// dark strokes that would vanish on a dark theme).
pub fn rasterize(data: &[u8], max_side: u32) -> Result<Rendered> {
    rasterize_with(data, max_side, true)
}

/// Same as [`rasterize`]; `checkerboard = false` keeps transparency.
pub fn rasterize_with(data: &[u8], max_side: u32, checkerboard: bool) -> Result<Rendered> {
    anyhow::ensure!(data.len() <= MAX_SVG_BYTES, "SVG too large ({} bytes)", data.len());
    let opt = usvg::Options { fontdb: fontdb(), ..usvg::Options::default() };
    let tree = usvg::Tree::from_data(data, &opt).context("invalid SVG")?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    anyhow::ensure!(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0, "SVG has no size");
    let scale = max_side.max(1) as f32 / w.max(h);
    let pw = ((w * scale).round() as u32).clamp(1, max_side.max(1));
    let ph = ((h * scale).round() as u32).clamp(1, max_side.max(1));
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph).context("cannot allocate bitmap")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());

    let mut out = RgbaImage::new(pw, ph);
    for (i, px) in pixmap.pixels().iter().enumerate() {
        let c = px.demultiply();
        let (x, y) = (i as u32 % pw, i as u32 / pw);
        let p = if checkerboard {
            let bg: u16 = if (x / 12 + y / 12) % 2 == 0 { 255 } else { 220 };
            let a = c.alpha() as u16;
            let mix = |v: u8| ((v as u16 * a + bg * (255 - a)) / 255) as u8;
            Rgba([mix(c.red()), mix(c.green()), mix(c.blue()), 255])
        } else {
            Rgba([c.red(), c.green(), c.blue(), c.alpha()])
        };
        out.put_pixel(x, y, p);
    }
    Ok(Rendered { image: DynamicImage::ImageRgba8(out), width: w, height: h })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CIRCLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20">
        <rect width="40" height="20" fill="#ff0000"/><circle cx="30" cy="10" r="8" fill="#0000ff"/></svg>"##;

    #[test]
    fn renders_at_requested_size_with_colors() {
        let r = rasterize(CIRCLE.as_bytes(), 400).unwrap();
        assert_eq!((r.width, r.height), (40.0, 20.0));
        let img = r.image.to_rgba8();
        assert_eq!(img.dimensions(), (400, 200));
        assert_eq!(img.get_pixel(20, 100).0, [255, 0, 0, 255], "red background");
        assert_eq!(img.get_pixel(300, 100).0, [0, 0, 255, 255], "blue circle");
    }

    #[test]
    fn svgz_and_transparency() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#).unwrap();
        let data = gz.finish().unwrap();
        let r = rasterize_with(&data, 50, false).unwrap();
        assert_eq!(r.image.to_rgba8().get_pixel(5, 5).0[3], 0, "empty SVG stays transparent");
        let r = rasterize(&data, 50).unwrap();
        assert_eq!(r.image.to_rgba8().get_pixel(5, 5).0[3], 255, "checkerboard is opaque");
    }

    #[test]
    fn rejects_garbage() {
        assert!(rasterize(b"not an svg", 100).is_err());
        assert!(rasterize(b"<svg xmlns='http://www.w3.org/2000/svg' width='0' height='0'/>", 100).is_err());
    }
}

#[cfg(test)]
mod timing {
    #[test]
    #[ignore]
    fn font_load_time() {
        let t = std::time::Instant::now();
        let db = super::fontdb();
        eprintln!("fontdb: {} faces in {:?}", db.len(), t.elapsed());
    }
}

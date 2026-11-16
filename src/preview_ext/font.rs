//! Font files: a rendered sample sheet plus name / style / glyph count.
//!
//! Rendering goes through resvg (an SVG of text lines with a font database that
//! holds only the previewed font) and metadata through skrifa, both already used
//! for SVG previews, so no extra font stack is pulled in.

use anyhow::{Context, Result};
use image::DynamicImage;
use resvg::usvg::{self, fontdb};
use skrifa::MetadataProvider;
use skrifa::raw::TableProvider;
use std::sync::Arc;

pub fn is_font_ext(ext: &str) -> bool {
    matches!(ext, "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2")
}

/// Font files above this size are not rendered.
pub const MAX_FONT_BYTES: usize = 32 << 20;

const WIDTH: u32 = 1400;
const PANGRAM: &str = "The quick brown fox jumps over the lazy dog";

pub struct Sheet {
    pub image: DynamicImage,
    pub info: Vec<(String, String)>,
}

fn check_format(data: &[u8]) -> Result<()> {
    anyhow::ensure!(data.len() <= MAX_FONT_BYTES, "font file too large");
    if data.starts_with(b"wOFF") || data.starts_with(b"wOF2") {
        anyhow::bail!("WOFF / WOFF2 web fonts are compressed; only TTF / OTF / TTC can be previewed");
    }
    Ok(())
}

/// Font database containing just this font (first face of a collection).
fn database(data: &[u8]) -> Result<(fontdb::Database, String)> {
    let mut db = fontdb::Database::new();
    db.load_font_data(data.to_vec());
    let face = db.faces().next().context("not a font this previewer can read")?;
    let family = face.families.first().map(|(f, _)| f.clone()).unwrap_or_else(|| face.post_script_name.clone());
    Ok((db, family))
}

/// Family, style and other details read from the font tables.
pub fn info(data: &[u8]) -> Result<Vec<(String, String)>> {
    check_format(data)?;
    let (db, family) = database(data)?;
    let face = db.faces().next().context("no face")?;
    let mut v = vec![("Family".to_string(), family)];
    let style = match face.style {
        fontdb::Style::Normal => "normal",
        fontdb::Style::Italic => "italic",
        fontdb::Style::Oblique => "oblique",
    };
    v.push(("Style".into(), format!("{style}, weight {}", face.weight.0)));
    if !face.post_script_name.is_empty() {
        v.push(("PostScript".into(), face.post_script_name.clone()));
    }
    if face.monospaced {
        v.push(("Spacing".into(), "monospaced".into()));
    }
    if let Ok(font) = skrifa::FontRef::from_index(data, 0) {
        if let Ok(maxp) = font.maxp() {
            v.push(("Glyphs".into(), maxp.num_glyphs().to_string()));
        }
        let axes: Vec<String> = font.axes().iter().map(|a| a.tag().to_string()).collect();
        if !axes.is_empty() {
            v.push(("Variable axes".into(), axes.join(", ")));
        }
    }
    if let Ok(skrifa::raw::FileRef::Collection(c)) = skrifa::raw::FileRef::new(data) {
        v.push(("Collection".into(), format!("{} faces (showing the first)", c.len())));
    }
    Ok(v)
}

fn xml_escape(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Render the sample sheet (black on white).
pub fn sample_sheet(data: &[u8]) -> Result<Sheet> {
    check_format(data)?;
    let (db, family) = database(data)?;
    let info = info(data).unwrap_or_default();
    let title = xml_escape(&family);
    let fam = xml_escape(&family);
    let lines: Vec<(String, f32)> = vec![
        (title, 44.0),
        (PANGRAM.into(), 14.0),
        (PANGRAM.into(), 20.0),
        (PANGRAM.into(), 28.0),
        (PANGRAM.into(), 40.0),
        (PANGRAM.into(), 56.0),
        ("ABCDEFGHIJKLMNOPQRSTUVWXYZ".into(), 28.0),
        ("abcdefghijklmnopqrstuvwxyz".into(), 28.0),
        (xml_escape("0123456789 !@#$%&*()[]{}<>?/\\|;:'\",.-_+=~"), 28.0),
    ];
    let mut y = 24.0;
    let mut body = String::new();
    for (text, size) in &lines {
        y += size * 1.25;
        body.push_str(&format!(
            "<text x=\"24\" y=\"{y:.1}\" font-family=\"{fam}\" font-size=\"{size}\" fill=\"#000\">{text}</text>\n"
        ));
        y += size * 0.35;
    }
    let height = (y + 24.0).ceil() as u32;
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{WIDTH}\" height=\"{height}\">\
         <rect width=\"100%\" height=\"100%\" fill=\"#fff\"/>{body}</svg>"
    );
    let opt = usvg::Options { fontdb: Arc::new(db), font_family: family.clone(), ..usvg::Options::default() };
    let tree = usvg::Tree::from_data(svg.as_bytes(), &opt).context("cannot lay out the sample text")?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(WIDTH, height).context("cannot allocate bitmap")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let mut img = image::RgbImage::new(WIDTH, height);
    for (i, px) in pixmap.pixels().iter().enumerate() {
        let c = px.demultiply();
        img.put_pixel(i as u32 % WIDTH, i as u32 / WIDTH, image::Rgb([c.red(), c.green(), c.blue()]));
    }
    Ok(Sheet { image: DynamicImage::ImageRgb8(img), info })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_font() -> Option<Vec<u8>> {
        [
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Geneva.ttf",
            "/Library/Fonts/Arial Unicode.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ]
        .iter()
        .find_map(|p| std::fs::read(p).ok())
    }

    #[test]
    fn renders_a_system_font() {
        let Some(data) = system_font() else { return };
        let sheet = sample_sheet(&data).expect("sheet");
        assert_eq!(sheet.image.width(), WIDTH);
        let rgb = sheet.image.to_rgb8();
        assert!(rgb.pixels().filter(|p| p.0[0] < 64).count() > 500, "text was drawn");
        assert!(sheet.info.iter().any(|(k, _)| k == "Glyphs"));
        assert!(sheet.info.iter().any(|(k, _)| k == "Family"));
    }

    #[test]
    fn rejects_garbage_and_woff() {
        assert!(sample_sheet(b"definitely not a font").is_err());
        assert!(sample_sheet(b"wOF2\0\0\0\0").is_err());
        assert!(info(b"").is_err());
    }
}

//! Image actions: rotate, flip, resize, convert, strip metadata and auto-orient.
//!
//! The pixel work is a pure function ([`process`]) so it can be tested without a
//! UI. The App side collects the selection, asks for options and runs a background
//! job that reads and writes through the VFS (local disk, archives, SFTP, S3...).
//!
//! JPEG rotation: by default rotate/flip only rewrites the EXIF orientation tag,
//! which is lossless and instant; every modern viewer honors it. A tag is added
//! when the EXIF data has none, and a minimal EXIF segment when there is no EXIF
//! at all. Only for malformed files, or when lossless rotation is turned off
//! (`image_lossless_jpeg_rotate = false`), are the pixels re-encoded at
//! `image_jpeg_quality`, keeping the EXIF and ICC data (orientation reset to 1).

use crate::app::{App, AppEvent};
use crate::dialog::{self, Form, ListAction, ListDialog, ListItem};
use crate::ops::{JobMsg, JobProgress};
use crate::undo::UndoOp;
use crate::vfs::{Location, Vfs, VfsHandle};
use anyhow::{Context, Result, anyhow, bail};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Largest input file we load into memory.
pub const MAX_INPUT_BYTES: usize = 256 << 20;
/// Largest accepted width / height (decoder limit, like the preview).
pub const MAX_SIDE: u32 = 16384;
const MAX_ALLOC: u64 = 1 << 30;

// ------------------------------------------------------------------ model

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    RotateRight,
    RotateLeft,
    Rotate180,
    FlipH,
    FlipV,
}

impl Transform {
    pub fn orientation(self) -> Orientation {
        match self {
            Transform::RotateRight => Orientation::Rotate90,
            Transform::RotateLeft => Orientation::Rotate270,
            Transform::Rotate180 => Orientation::Rotate180,
            Transform::FlipH => Orientation::FlipHorizontal,
            Transform::FlipV => Orientation::FlipVertical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Transform::RotateRight => "Rotate right",
            Transform::RotateLeft => "Rotate left",
            Transform::Rotate180 => "Rotate 180",
            Transform::FlipH => "Flip horizontally",
            Transform::FlipV => "Flip vertically",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResizeMode {
    /// Fit inside w x h keeping the aspect ratio (0 = unlimited).
    Fit {
        w: u32,
        h: u32,
    },
    /// Exactly w x h (stretch); one side 0 = derive it from the aspect ratio.
    Exact {
        w: u32,
        h: u32,
    },
    Percent(f64),
    /// Crop the center to the aspect ratio of w x h, then scale to exactly w x h.
    Fill {
        w: u32,
        h: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeOpts {
    pub mode: ResizeMode,
    /// Never enlarge: images already smaller than the target are left alone.
    pub only_shrink: bool,
    pub filter: FilterType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutFormat {
    Png,
    Jpeg,
    WebP,
    Bmp,
    Tiff,
    Gif,
    Qoi,
    Ico,
}

impl OutFormat {
    pub const ALL: &'static [OutFormat] = &[
        OutFormat::Png,
        OutFormat::Jpeg,
        OutFormat::WebP,
        OutFormat::Bmp,
        OutFormat::Tiff,
        OutFormat::Gif,
        OutFormat::Qoi,
        OutFormat::Ico,
    ];

    pub fn ext(self) -> &'static str {
        match self {
            OutFormat::Png => "png",
            OutFormat::Jpeg => "jpg",
            OutFormat::WebP => "webp",
            OutFormat::Bmp => "bmp",
            OutFormat::Tiff => "tiff",
            OutFormat::Gif => "gif",
            OutFormat::Qoi => "qoi",
            OutFormat::Ico => "ico",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutFormat::Png => "PNG",
            OutFormat::Jpeg => "JPEG (quality below)",
            OutFormat::WebP => "WebP (lossless only)",
            OutFormat::Bmp => "BMP",
            OutFormat::Tiff => "TIFF",
            OutFormat::Gif => "GIF (single frame, 256 colors)",
            OutFormat::Qoi => "QOI",
            OutFormat::Ico => "ICO (max 256x256)",
        }
    }

    /// The format to re-encode an input of `f` in, if we can write it.
    pub fn from_input(f: ImageFormat) -> Option<OutFormat> {
        Some(match f {
            ImageFormat::Png => OutFormat::Png,
            ImageFormat::Jpeg => OutFormat::Jpeg,
            ImageFormat::WebP => OutFormat::WebP,
            ImageFormat::Bmp => OutFormat::Bmp,
            ImageFormat::Tiff => OutFormat::Tiff,
            ImageFormat::Gif => OutFormat::Gif,
            ImageFormat::Qoi => OutFormat::Qoi,
            ImageFormat::Ico => OutFormat::Ico,
            _ => return None,
        })
    }

    fn matches_ext(self, ext: &str) -> bool {
        let e = ext.to_ascii_lowercase();
        match self {
            OutFormat::Jpeg => e == "jpg" || e == "jpeg",
            OutFormat::Tiff => e == "tif" || e == "tiff",
            other => e == other.ext(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Transform(Transform),
    Resize(ResizeOpts),
    /// `svg_size`: longest side in pixels when the input is an SVG.
    Convert {
        format: OutFormat,
        svg_size: u32,
    },
    Strip,
    AutoOrient,
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Action::Transform(t) => t.label().to_string(),
            Action::Resize(_) => "Resize".into(),
            Action::Convert { format, .. } => format!("Convert to {}", format.ext().to_uppercase()),
            Action::Strip => "Strip metadata".into(),
            Action::AutoOrient => "Auto-orient".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Replace the original (for a format change: write the new file, remove the old).
    Overwrite,
    /// Write `stem<suffix>.ext` next to the original; `{w}` / `{h}` become the new size.
    Suffix(String),
    /// Write into this directory (relative paths are relative to the image's directory).
    Dir(PathBuf),
    /// Convert only: write `stem.<new ext>` next to the original, keep the original.
    NewExtension,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub action: Action,
    pub output: Output,
    pub jpeg_quality: u8,
    pub lossless_jpeg_rotate: bool,
}

/// Result of processing one file.
#[derive(Debug)]
pub struct Processed {
    pub data: Vec<u8>,
    /// New extension when the format changed.
    pub ext: Option<&'static str>,
    pub width: u32,
    pub height: u32,
    /// Nothing to do (already upright, already that size...).
    pub unchanged: bool,
    /// True when a JPEG was rotated by rewriting the EXIF orientation tag only.
    pub lossless: bool,
}

// ------------------------------------------------------------------ orientation algebra

fn probe() -> DynamicImage {
    // 3x2 image with distinct pixels: every orientation produces a different result.
    DynamicImage::ImageLuma8(image::GrayImage::from_fn(3, 2, |x, y| image::Luma([(y * 3 + x) as u8 * 30])))
}

const ALL_ORIENTATIONS: [Orientation; 8] = [
    Orientation::NoTransforms,
    Orientation::Rotate90,
    Orientation::Rotate180,
    Orientation::Rotate270,
    Orientation::FlipHorizontal,
    Orientation::FlipVertical,
    Orientation::Rotate90FlipH,
    Orientation::Rotate270FlipH,
];

/// The single orientation equal to applying `first`, then `then`.
pub fn compose(first: Orientation, then: Orientation) -> Orientation {
    let mut want = probe();
    want.apply_orientation(first);
    want.apply_orientation(then);
    ALL_ORIENTATIONS
        .into_iter()
        .find(|o| {
            let mut i = probe();
            i.apply_orientation(*o);
            i == want
        })
        .unwrap_or(Orientation::NoTransforms)
}

fn swaps_axes(o: Orientation) -> bool {
    matches!(
        o,
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    )
}

// ------------------------------------------------------------------ JPEG EXIF orientation

/// Offset of the orientation value inside a TIFF-structured EXIF block, and whether
/// the block is big-endian. None if there is no (valid SHORT) orientation tag.
pub fn find_orientation_tag(tiff: &[u8]) -> Option<(usize, bool)> {
    if tiff.len() < 8 {
        return None;
    }
    let be = match &tiff[..2] {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if be { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for i in 0..count.min(512) {
        let e = ifd.checked_add(2 + 12 * i)?;
        if u16_at(e)? == 0x0112 {
            // type SHORT (3), count 1: the value sits in the first 2 bytes of the value field
            if u16_at(e + 2)? != 3 || u32_at(e + 4)? != 1 {
                return None;
            }
            u16_at(e + 8)?;
            return Some((e + 8, be));
        }
    }
    None
}

/// Add an orientation entry to IFD0 of a TIFF-structured EXIF block that has
/// none: a copy of IFD0 with the new entry (kept sorted by tag) is appended at
/// the end and the header points to it. Value offsets are relative to the start
/// of the block and nothing moves, so all other data stays valid.
pub fn add_orientation_tag(tiff: &[u8], value: u8) -> Option<Vec<u8>> {
    if tiff.len() < 8 || find_orientation_tag(tiff).is_some() {
        return None;
    }
    let be = match &tiff[..2] {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let rd16 = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if be { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    };
    let rd32 = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    };
    let w16 = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
    let w32 = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
    if rd16(2)? != 42 {
        return None;
    }
    let ifd = rd32(4)? as usize;
    let count = rd16(ifd)? as usize;
    if count > 1000 {
        return None;
    }
    let entries = tiff.get(ifd + 2..ifd + 2 + 12 * count)?;
    let next = rd32(ifd + 2 + 12 * count)?;
    let mut new_entry = Vec::with_capacity(12);
    new_entry.extend_from_slice(&w16(0x0112));
    new_entry.extend_from_slice(&w16(3));
    new_entry.extend_from_slice(&w32(1));
    new_entry.extend_from_slice(&w16(u16::from(value)));
    new_entry.extend_from_slice(&[0, 0]);
    let mut out = tiff.to_vec();
    if out.len() % 2 == 1 {
        out.push(0); // IFDs start on a word boundary
    }
    let new_ifd = u32::try_from(out.len()).ok()?;
    out.extend_from_slice(&w16(u16::try_from(count + 1).ok()?));
    let mut inserted = false;
    for e in entries.as_chunks::<12>().0 {
        let tag = if be { u16::from_be_bytes([e[0], e[1]]) } else { u16::from_le_bytes([e[0], e[1]]) };
        if !inserted && tag > 0x0112 {
            out.extend_from_slice(&new_entry);
            inserted = true;
        }
        out.extend_from_slice(e);
    }
    if !inserted {
        out.extend_from_slice(&new_entry);
    }
    out.extend_from_slice(&w32(next));
    out[4..8].copy_from_slice(&w32(new_ifd));
    Some(out)
}

/// Minimal APP1 Exif segment holding only an orientation tag.
fn minimal_exif_app1(o: u8) -> Vec<u8> {
    let mut p = Vec::with_capacity(36);
    p.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x22]); // marker + length 34
    p.extend_from_slice(b"Exif\0\0");
    p.extend_from_slice(b"MM\x00\x2a\x00\x00\x00\x08"); // big-endian TIFF, IFD0 at 8
    p.extend_from_slice(&[0x00, 0x01]); // one entry
    p.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, o, 0x00, 0x00]);
    p.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no next IFD
    p
}

/// Set the EXIF orientation of a JPEG without touching the compressed image data.
/// Patches an existing tag, adds one to existing EXIF data, or inserts a minimal
/// EXIF segment. Returns None for malformed files (the caller re-encodes).
pub fn set_jpeg_orientation(data: &[u8], o: Orientation) -> Option<Vec<u8>> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let value = o.to_exif();
    let mut pos = 2;
    let mut insert_at = 2;
    let mut first = true;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        if marker == 0xFF {
            pos += 1; // fill byte
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break; // start of scan / end of image: no EXIF found
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            pos += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if len < 2 || pos + 2 + len > data.len() {
            return None;
        }
        let payload = &data[pos + 4..pos + 2 + len];
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let tiff_start = pos + 4 + 6;
            let tiff = &payload[6..];
            if let Some((off, be)) = find_orientation_tag(tiff) {
                let mut out = data.to_vec();
                let v = u16::from(value);
                let bytes = if be { v.to_be_bytes() } else { v.to_le_bytes() };
                out[tiff_start + off..tiff_start + off + 2].copy_from_slice(&bytes);
                return Some(out);
            }
            // EXIF without an orientation tag: rebuild this segment with one added.
            let new_tiff = add_orientation_tag(tiff, value)?;
            let seg_len = u16::try_from(2 + 6 + new_tiff.len()).ok()?;
            let mut out = Vec::with_capacity(data.len() + new_tiff.len() - tiff.len());
            out.extend_from_slice(&data[..pos]);
            out.extend_from_slice(&[0xFF, 0xE1]);
            out.extend_from_slice(&seg_len.to_be_bytes());
            out.extend_from_slice(b"Exif\0\0");
            out.extend_from_slice(&new_tiff);
            out.extend_from_slice(&data[pos + 2 + len..]);
            return Some(out);
        }
        // Keep a JFIF APP0 first; the new EXIF segment goes right after it.
        if first && marker == 0xE0 {
            insert_at = pos + 2 + len;
        }
        first = false;
        pos += 2 + len;
    }
    let mut out = Vec::with_capacity(data.len() + 36);
    out.extend_from_slice(&data[..insert_at]);
    out.extend_from_slice(&minimal_exif_app1(value));
    out.extend_from_slice(&data[insert_at..]);
    Some(out)
}

// ------------------------------------------------------------------ geometry

/// New size for a resize, or the original size when nothing should change.
pub fn target_size(iw: u32, ih: u32, opts: &ResizeOpts) -> Result<(u32, u32)> {
    let (fw, fh) = (iw.max(1) as f64, ih.max(1) as f64);
    let r = |v: f64| (v.round() as u32).clamp(1, MAX_SIDE);
    let (tw, th) = match opts.mode {
        ResizeMode::Percent(p) => {
            if !(p > 0.0 && p <= 10_000.0) {
                bail!("percent must be between 0 and 10000");
            }
            (r(fw * p / 100.0), r(fh * p / 100.0))
        }
        ResizeMode::Fit { w, h } => {
            if w == 0 && h == 0 {
                bail!("give a width, a height or both");
            }
            let sw = if w == 0 { f64::INFINITY } else { w as f64 / fw };
            let sh = if h == 0 { f64::INFINITY } else { h as f64 / fh };
            let s = sw.min(sh);
            (r(fw * s), r(fh * s))
        }
        ResizeMode::Exact { w, h } => match (w, h) {
            (0, 0) => bail!("give a width, a height or both"),
            (w, 0) => (r(w as f64), r(fh * w as f64 / fw)),
            (0, h) => (r(fw * h as f64 / fh), r(h as f64)),
            (w, h) => (r(w as f64), r(h as f64)),
        },
        ResizeMode::Fill { w, h } => {
            if w == 0 || h == 0 {
                bail!("fill needs both a width and a height");
            }
            (r(w as f64), r(h as f64))
        }
    };
    if opts.only_shrink && tw >= iw && th >= ih {
        return Ok((iw, ih));
    }
    Ok((tw, th))
}

/// Center crop rectangle (x, y, w, h) with the aspect ratio of tw x th.
pub fn fill_crop(iw: u32, ih: u32, tw: u32, th: u32) -> (u32, u32, u32, u32) {
    let ta = tw as f64 / th.max(1) as f64;
    let ia = iw as f64 / ih.max(1) as f64;
    if ia > ta {
        let cw = ((ih as f64 * ta).round() as u32).clamp(1, iw);
        ((iw - cw) / 2, 0, cw, ih)
    } else {
        let ch = ((iw as f64 / ta).round() as u32).clamp(1, ih);
        (0, (ih - ch) / 2, iw, ch)
    }
}

// ------------------------------------------------------------------ naming

/// Where the result of processing `src` goes.
pub fn output_path(src: &Path, output: &Output, new_ext: Option<&str>, dims: (u32, u32)) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into());
    let orig_ext = src.extension().map(|e| e.to_string_lossy().into_owned());
    let ext = new_ext.map(str::to_string).or(orig_ext);
    let file = |stem: &str| match &ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem.to_string(),
    };
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    match output {
        Output::Overwrite => dir.join(file(&stem)),
        Output::NewExtension => {
            let p = dir.join(file(&stem));
            if p == src { dir.join(file(&format!("{stem}_converted"))) } else { p }
        }
        Output::Suffix(t) => {
            let t = if t.is_empty() { "_edited" } else { t.as_str() };
            let suffix = t.replace("{w}", &dims.0.to_string()).replace("{h}", &dims.1.to_string());
            dir.join(file(&format!("{stem}{suffix}")))
        }
        Output::Dir(d) => {
            let d = if d.is_absolute() { d.clone() } else { dir.join(d) };
            crate::vfs::normalize(&d).join(file(&stem))
        }
    }
}

/// `name (1).ext`, `name (2).ext`... until `exists` says no.
pub fn unique_path(p: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if !exists(p) {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 1..10_000 {
        let c = p.with_file_name(format!("{stem} ({i}){ext}"));
        if !exists(&c) {
            return c;
        }
    }
    p.to_path_buf()
}

pub fn is_image_name(name: &str) -> bool {
    let ext = crate::icons::ext_of(name);
    crate::icons::IMAGE_EXTS.contains(&ext.as_str())
}

// ------------------------------------------------------------------ processing

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_SIDE);
    l.max_image_height = Some(MAX_SIDE);
    l.max_alloc = Some(MAX_ALLOC);
    l
}

struct Decoded {
    img: DynamicImage,
    format: Option<ImageFormat>,
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
    orientation: Orientation,
}

fn decode(data: &[u8], name: &str, svg_size: u32) -> Result<Decoded> {
    if crate::svg::is_svg_name(name) {
        let r = crate::svg::rasterize_with(data, svg_size.clamp(1, MAX_SIDE), false)?;
        return Ok(Decoded {
            img: r.image,
            format: None,
            exif: None,
            icc: None,
            orientation: Orientation::NoTransforms,
        });
    }
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().context("cannot read image")?;
    let format = reader.format().ok_or_else(|| anyhow!("not an image (unknown format)"))?;
    reader.limits(limits());
    if format == ImageFormat::Gif && gif_frames(data) > 1 {
        bail!("animated GIFs are not supported (only the first frame would be kept)");
    }
    let mut dec = reader.into_decoder().context("cannot decode image")?;
    let exif = dec.exif_metadata().ok().flatten();
    let icc = dec.icc_profile().ok().flatten();
    let orientation = dec.orientation().unwrap_or(Orientation::NoTransforms);
    let img = DynamicImage::from_decoder(dec).context("cannot decode image")?;
    Ok(Decoded { img, format: Some(format), exif, icc, orientation })
}

fn gif_frames(data: &[u8]) -> usize {
    use image::AnimationDecoder;
    match image::codecs::gif::GifDecoder::new(Cursor::new(data)) {
        Ok(d) => d.into_frames().take(2).count(),
        Err(_) => 0,
    }
}

/// Composite onto white and drop alpha (JPEG has no transparency).
fn flatten_rgb(img: &DynamicImage) -> image::RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let a = p[3] as u16;
        let mix = |c: u8| ((c as u16 * a + 255 * (255 - a)) / 255) as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

/// Encode `img` as `fmt`. EXIF / ICC are written for JPEG, PNG and WebP.
pub fn encode(
    img: &DynamicImage,
    fmt: OutFormat,
    quality: u8,
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match fmt {
        OutFormat::Jpeg => {
            let rgb = flatten_rgb(img);
            let mut e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
            if let Some(x) = exif {
                let _ = e.set_exif_metadata(x);
            }
            if let Some(i) = icc {
                let _ = e.set_icc_profile(i);
            }
            e.write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)?;
        }
        OutFormat::Png => {
            let mut e = image::codecs::png::PngEncoder::new(&mut out);
            if let Some(x) = exif {
                let _ = e.set_exif_metadata(x);
            }
            if let Some(i) = icc {
                let _ = e.set_icc_profile(i);
            }
            img.write_with_encoder(e)?;
        }
        OutFormat::WebP => {
            let mut e = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            if let Some(x) = exif {
                let _ = e.set_exif_metadata(x);
            }
            if let Some(i) = icc {
                let _ = e.set_icc_profile(i);
            }
            let rgba = DynamicImage::ImageRgba8(img.to_rgba8());
            rgba.write_with_encoder(e)?;
        }
        OutFormat::Ico => {
            let small = if img.width() > 256 || img.height() > 256 { img.thumbnail(256, 256) } else { img.clone() };
            DynamicImage::ImageRgba8(small.to_rgba8()).write_to(&mut Cursor::new(&mut out), ImageFormat::Ico)?;
        }
        OutFormat::Gif => {
            DynamicImage::ImageRgba8(img.to_rgba8()).write_to(&mut Cursor::new(&mut out), ImageFormat::Gif)?;
        }
        OutFormat::Bmp | OutFormat::Qoi => {
            let f = if fmt == OutFormat::Bmp { ImageFormat::Bmp } else { ImageFormat::Qoi };
            let eight = if img.color().has_alpha() {
                DynamicImage::ImageRgba8(img.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(img.to_rgb8())
            };
            eight.write_to(&mut Cursor::new(&mut out), f)?;
        }
        OutFormat::Tiff => {
            img.write_to(&mut Cursor::new(&mut out), ImageFormat::Tiff)?;
        }
    }
    Ok(out)
}

/// Apply `spec.action` to the image in `data` (file name `name`, used for the
/// extension and SVG detection).
pub fn process(data: &[u8], name: &str, spec: &Spec) -> Result<Processed> {
    let svg_size = match spec.action {
        Action::Convert { svg_size, .. } => svg_size,
        _ => 0,
    };
    if crate::svg::is_svg_name(name) && !matches!(spec.action, Action::Convert { .. }) {
        bail!("SVG is a vector format: use Convert to make a bitmap first");
    }
    let d = decode(data, name, svg_size.max(1))?;

    // Lossless JPEG rotation: only rewrite the EXIF orientation tag.
    if let (Action::Transform(t), Some(ImageFormat::Jpeg), true) = (&spec.action, d.format, spec.lossless_jpeg_rotate) {
        let o = compose(d.orientation, t.orientation());
        if let Some(out) = set_jpeg_orientation(data, o) {
            let (w, h) = if swaps_axes(o) { (d.img.height(), d.img.width()) } else { (d.img.width(), d.img.height()) };
            return Ok(Processed { data: out, ext: None, width: w, height: h, unchanged: false, lossless: true });
        }
    }

    let input_fmt = d.format;
    let mut exif = d.exif;
    let mut icc = d.icc;
    let mut img = d.img;
    let oriented = d.orientation != Orientation::NoTransforms;
    // Every re-encode bakes the EXIF orientation into the pixels and resets the tag,
    // so the result looks exactly like the preview.
    img.apply_orientation(d.orientation);
    if let Some(e) = exif.as_mut() {
        let _ = Orientation::remove_from_exif_chunk(e);
    }
    let mut out_fmt = input_fmt.and_then(OutFormat::from_input);
    let mut changed = oriented;
    match &spec.action {
        Action::Transform(t) => {
            img.apply_orientation(t.orientation());
            changed = true;
        }
        Action::Resize(opts) => {
            let (iw, ih) = (img.width(), img.height());
            let (tw, th) = target_size(iw, ih, opts)?;
            if (tw, th) != (iw, ih) {
                if let ResizeMode::Fill { .. } = opts.mode {
                    let (x, y, cw, ch) = fill_crop(iw, ih, tw, th);
                    img = img.crop_imm(x, y, cw, ch);
                }
                img = img.resize_exact(tw, th, opts.filter);
                changed = true;
            }
        }
        Action::Convert { format, .. } => {
            changed = changed || out_fmt != Some(*format);
            out_fmt = Some(*format);
        }
        Action::Strip => {
            changed = changed || exif.is_some() || icc.is_some();
            exif = None;
            icc = None;
        }
        Action::AutoOrient => {}
    }
    let fmt = out_fmt.ok_or_else(|| {
        anyhow!(
            "cannot write {} files; use Convert to save in another format",
            input_fmt.map(|f| format!("{f:?}")).unwrap_or_default()
        )
    })?;
    let (w, h) = (img.width(), img.height());
    if !changed {
        return Ok(Processed { data: Vec::new(), ext: None, width: w, height: h, unchanged: true, lossless: false });
    }
    let data = encode(&img, fmt, spec.jpeg_quality, exif, icc)?;
    let cur_ext = Path::new(name).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = if fmt.matches_ext(&cur_ext) { None } else { Some(fmt.ext()) };
    Ok(Processed { data, ext, width: w, height: h, unchanged: false, lossless: false })
}

// ------------------------------------------------------------------ writing

/// Write `data` to `target` through a temporary file next to it, then rename it
/// into place, so a crash or error never leaves a half-written image.
pub fn write_atomic(v: &mut dyn Vfs, target: &Path, data: &[u8], mode: u32) -> Result<()> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into());
    let tmp = target.with_file_name(format!(".{name}.pairdeck-tmp-{}", std::process::id()));
    let mode = if mode & 0o777 == 0 { 0o644 } else { mode & 0o7777 };
    if v.is_local() {
        use std::os::unix::fs::PermissionsExt;
        let res = (|| -> Result<()> {
            let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(data)?;
            f.sync_all()?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
            std::fs::rename(&tmp, target).with_context(|| format!("rename to {}", target.display()))
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return res;
    }
    let written = (|| -> Result<()> {
        let mut w = v.create_write(&tmp, mode)?;
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = v.remove_file(&tmp);
        return Err(e);
    }
    if v.rename(&tmp, target).is_ok() {
        return Ok(());
    }
    // No rename on this file system: write the target directly.
    let _ = v.remove_file(&tmp);
    let mut w = v.create_write(target, mode)?;
    w.write_all(data)?;
    w.flush()?;
    Ok(())
}

static BACKUP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Save the original bytes of an overwritten image so undo can restore them.
fn backup(name: &str, data: &[u8]) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("pairdeck-undo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let p = dir.join(format!("{}-{name}", BACKUP_SEQ.fetch_add(1, Relaxed)));
    std::fs::write(&p, data)?;
    Ok(p)
}

/// Undo of an image job: delete the files it created, put the backups back.
pub fn undo_restore(vfs: &VfsHandle, items: &[(PathBuf, PathBuf)], created: &[PathBuf]) -> Result<String> {
    let mut v = vfs.lock();
    for p in created {
        if !items.iter().any(|(t, _)| t == p) {
            let _ = v.remove_file(p);
        }
    }
    for (target, backup) in items {
        let data = std::fs::read(backup).with_context(|| format!("backup of {} is gone", target.display()))?;
        let mode = v.stat(target).map(|e| e.mode).unwrap_or(0o644);
        write_atomic(&mut **v, target, &data, mode)?;
    }
    let _ = v.commit();
    Ok(format!("Restored {} image(s)", items.len().max(created.len())))
}

/// Process one file through the VFS. Returns (new or changed path, backup info).
struct FileResult {
    created: Option<PathBuf>,
    restore: Option<(PathBuf, PathBuf)>,
    unchanged: bool,
}

fn process_file(vfs: &VfsHandle, path: &Path, spec: &Spec) -> Result<FileResult> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let entry = vfs.lock().stat(path)?;
    if entry.size as usize > MAX_INPUT_BYTES {
        bail!("{name}: larger than {} MiB", MAX_INPUT_BYTES >> 20);
    }
    let data = vfs.lock().read_head(path, MAX_INPUT_BYTES)?;
    let res = process(&data, &name, spec).with_context(|| name.clone())?;
    if res.unchanged {
        return Ok(FileResult { created: None, restore: None, unchanged: true });
    }
    let mut out = output_path(path, &spec.output, res.ext, (res.width, res.height));
    let replacing = matches!(spec.output, Output::Overwrite);
    if out != path {
        let exists = |p: &Path| vfs.lock().stat(p).is_ok();
        // Replacing with a new extension may land on an unrelated existing file: never clobber it.
        out = unique_path(&out, exists);
        if let Some(parent) = out.parent() {
            mkdir_all(vfs, parent)?;
        }
    }
    let mode = entry.mode;
    let mut restore = None;
    if out == path || replacing {
        restore = Some((path.to_path_buf(), backup(&name, &data)?));
    }
    write_atomic(&mut **vfs.lock(), &out, &res.data, mode)?;
    if replacing && out != path {
        vfs.lock().remove_file(path)?;
    }
    let created = if out != path { Some(out) } else { None };
    Ok(FileResult { created, restore, unchanged: false })
}

fn mkdir_all(vfs: &VfsHandle, dir: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut p = dir.to_path_buf();
    {
        let mut v = vfs.lock();
        while v.stat(&p).is_err() {
            missing.push(p.clone());
            if !p.pop() {
                break;
            }
        }
        for d in missing.iter().rev() {
            v.mkdir(d, 0o755)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ App side

/// Image files of the selection (marked files or the cursor file), plus the
/// number of non-image files skipped.
fn selected_images(app: &App) -> (Location, Vec<PathBuf>, usize) {
    let p = app.panel();
    let mut out = Vec::new();
    let mut skipped = 0;
    for e in p.selection() {
        if !e.is_dir() && is_image_name(&e.name) {
            out.push(p.loc.join(&e.name));
        } else {
            skipped += 1;
        }
    }
    (p.loc.clone(), out, skipped)
}

fn spec_for(app: &App, action: Action, output: Output) -> Spec {
    Spec {
        action,
        output,
        jpeg_quality: app.cfg.image_jpeg_quality,
        lossless_jpeg_rotate: app.cfg.image_lossless_jpeg_rotate,
    }
}

/// Start an image job on the current selection.
pub fn run(app: &mut App, spec: Spec) {
    let (loc, items, skipped) = selected_images(app);
    if items.is_empty() {
        app.flash_error(if skipped > 0 { "No image files in the selection" } else { "Nothing selected" });
        return;
    }
    if matches!(spec.output, Output::Overwrite) && !loc.vfs.lock().writable() {
        app.error("This file system is read-only");
        return;
    }
    let title = spec.action.label();
    let p = JobProgress::new(format!("{title} ({} image(s))", items.len()));
    p.total_files.store(items.len() as u64, Relaxed);
    p.scanning.store(false, Relaxed);
    app.track_job(p.clone(), true);
    app.panel_mut().marked.clear();
    let tx = app.tx.clone();
    std::thread::spawn(move || worker(p, loc.vfs, items, spec, skipped, title, tx));
}

fn worker(
    p: Arc<JobProgress>,
    vfs: VfsHandle,
    items: Vec<PathBuf>,
    spec: Spec,
    skipped: usize,
    title: String,
    tx: crossbeam_channel::Sender<AppEvent>,
) {
    let mut errors = Vec::new();
    let mut restores = Vec::new();
    let mut created = Vec::new();
    let (mut done, mut same) = (0, 0);
    for path in &items {
        if p.cancel.load(Relaxed) {
            break;
        }
        *p.current.lock().unwrap() = path.display().to_string();
        match process_file(&vfs, path, &spec) {
            Ok(r) if r.unchanged => same += 1,
            Ok(r) => {
                done += 1;
                restores.extend(r.restore);
                created.extend(r.created);
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        p.done_files.fetch_add(1, Relaxed);
    }
    let _ = vfs.lock().commit();
    p.finished.store(true, Relaxed);
    let mut summary = format!("{title}: {done} image(s) written");
    if same > 0 {
        summary.push_str(&format!(", {same} already done"));
    }
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} non-image(s) skipped"));
    }
    let undo = if restores.is_empty() && created.is_empty() {
        Vec::new()
    } else {
        vec![UndoOp::Restore { vfs, items: restores, created }]
    };
    let cancelled = p.cancel.load(Relaxed);
    let _ = tx.send(AppEvent::Job(JobMsg::Finished { id: p.id, summary, cancelled, errors, undo }));
}

/// Rotate or flip the selection in place (undo restores the originals).
pub fn transform(app: &mut App, t: Transform) {
    let spec = spec_for(app, Action::Transform(t), Output::Overwrite);
    run(app, spec);
}

/// Strip EXIF / ICC / text metadata, in place after a confirmation.
pub fn strip(app: &mut App) {
    let d = dialog::confirm(
        "Strip metadata",
        "Re-encode the selected images without EXIF, ICC and text metadata?\nJPEGs are re-encoded at the configured quality. Undo restores the originals.",
        |app| {
            let spec = spec_for(app, Action::Strip, Output::Overwrite);
            run(app, spec);
        },
    );
    app.push_dialog(d);
}

/// Apply the EXIF orientation to the pixels and reset the tag.
pub fn auto_orient(app: &mut App) {
    let spec = spec_for(app, Action::AutoOrient, Output::Overwrite);
    run(app, spec);
}

const FILTERS: [(&str, FilterType); 4] = [
    ("Lanczos3 (sharpest)", FilterType::Lanczos3),
    ("Catmull-Rom", FilterType::CatmullRom),
    ("Bilinear", FilterType::Triangle),
    ("Nearest (pixel art)", FilterType::Nearest),
];

fn parse_u32(s: &str) -> Result<u32> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    s.parse::<u32>().map_err(|_| anyhow!("not a number: {s}"))
}

fn output_from(choice: usize, suffix: &str, dir: &str, convert: bool) -> Result<Output> {
    Ok(match (choice, convert) {
        (0, false) => Output::Overwrite,
        (1, false) => Output::Suffix(suffix.to_string()),
        (0, true) => Output::NewExtension,
        (1, true) => Output::Overwrite,
        _ => {
            let d = dir.trim();
            if d.is_empty() {
                bail!("give an output directory");
            }
            Output::Dir(PathBuf::from(crate::util::expand_tilde(d)))
        }
    })
}

/// History-backed input: preselected (typing replaces it) and remembers the last value.
fn field(form: Form, app: &mut App, id: &str, label: &str, default: &str) -> Form {
    let key = format!("image_{id}");
    let hist = app.state.input_hist(&key).clone();
    let value = hist.last().cloned().unwrap_or_else(|| default.to_string());
    form.input_hist(id, label, &value, &key, hist)
}

pub fn resize_dialog(app: &mut App) {
    let q = app.cfg.image_jpeg_quality.to_string();
    let filters: Vec<&str> = FILTERS.iter().map(|f| f.0).collect();
    let d = Form::new("Resize images").width(60).radio(
        "mode",
        "Mode:",
        &["Fit within width x height", "Exact size (stretch)", "Percent", "Fill: crop to size"],
        0,
    );
    let d = field(d, app, "w", "Width (px, empty = auto):", "1920");
    let d = field(d, app, "h", "Height (px, empty = auto):", "1080");
    let d = field(d, app, "pct", "Percent:", "50")
        .check("shrink", "Only shrink (never enlarge)", true)
        .radio("filter", "Filter:", &filters, 0)
        .radio("out", "Output:", &["Overwrite original (undo keeps a backup)", "Add suffix", "Into directory"], 1);
    let d = field(d, app, "suffix", "Suffix ({w} and {h} = new size):", "_{w}x{h}");
    let d = field(d, app, "dir", "Directory:", "resized");
    let d = field(d, app, "q", "JPEG quality (1-100):", &q).buttons(&["Resize", "Cancel"], 0).on_submit(|app, r| {
        if r.button != 0 {
            return;
        }
        let res = (|| -> Result<Spec> {
            let (w, h) = (parse_u32(&r.text("w"))?, parse_u32(&r.text("h"))?);
            let mode = match r.radio("mode") {
                0 => ResizeMode::Fit { w, h },
                1 => ResizeMode::Exact { w, h },
                2 => ResizeMode::Percent(r.text("pct").trim().parse::<f64>().map_err(|_| anyhow!("bad percent"))?),
                _ => ResizeMode::Fill { w, h },
            };
            let opts = ResizeOpts { mode, only_shrink: r.check("shrink"), filter: FILTERS[r.radio("filter")].1 };
            // Validate the numbers now rather than once per file.
            target_size(1000, 1000, &opts)?;
            let output = output_from(r.radio("out"), &r.text("suffix"), &r.text("dir"), false)?;
            let mut spec = spec_for(app, Action::Resize(opts), output);
            spec.jpeg_quality = parse_u32(&r.text("q"))?.clamp(1, 100) as u8;
            Ok(spec)
        })();
        match res {
            Ok(spec) => run(app, spec),
            Err(e) => app.error(format!("{e:#}")),
        }
    });
    app.push_dialog(Box::new(d));
}

pub fn convert_dialog(app: &mut App) {
    let q = app.cfg.image_jpeg_quality.to_string();
    let labels: Vec<&str> = OutFormat::ALL.iter().map(|f| f.label()).collect();
    let d = Form::new("Convert images").width(60).radio("fmt", "Format:", &labels, 0);
    let d = field(d, app, "q", "JPEG quality (1-100):", &q);
    let d = field(d, app, "svg", "SVG input: longest side in px:", "1024").radio(
        "out",
        "Output:",
        &["Next to the original (keep it)", "Replace the original (undo keeps a backup)", "Into directory"],
        0,
    );
    let d = field(d, app, "cdir", "Directory:", "converted").buttons(&["Convert", "Cancel"], 0).on_submit(|app, r| {
        if r.button != 0 {
            return;
        }
        let res = (|| -> Result<Spec> {
            let format = OutFormat::ALL[r.radio("fmt")];
            let svg_size = parse_u32(&r.text("svg"))?.clamp(16, MAX_SIDE);
            let output = output_from(r.radio("out"), "", &r.text("cdir"), true)?;
            let mut spec = spec_for(app, Action::Convert { format, svg_size }, output);
            spec.jpeg_quality = parse_u32(&r.text("q"))?.clamp(1, 100) as u8;
            Ok(spec)
        })();
        match res {
            Ok(spec) => run(app, spec),
            Err(e) => app.error(format!("{e:#}")),
        }
    });
    app.push_dialog(Box::new(d));
}

/// The "Image actions..." chooser: every action in one list.
pub fn open(app: &mut App) {
    let key = |c: crate::keymap::Command| app.keymap.label_for(c).unwrap_or_default();
    use crate::keymap::Command as C;
    let entries: Vec<(String, String)> = vec![
        ("Rotate right (90 clockwise)".into(), key(C::ImageRotateRight)),
        ("Rotate left (90 counter-clockwise)".into(), key(C::ImageRotateLeft)),
        ("Rotate 180".into(), key(C::ImageRotate180)),
        ("Flip horizontally".into(), key(C::ImageFlipH)),
        ("Flip vertically".into(), key(C::ImageFlipV)),
        ("Resize...".into(), key(C::ImageResize)),
        ("Convert format...".into(), key(C::ImageConvert)),
        ("Strip metadata...".into(), key(C::ImageStrip)),
        ("Auto-orient (apply EXIF rotation)".into(), key(C::ImageAutoOrient)),
    ];
    let items: Vec<ListItem> = entries.iter().map(|(l, k)| ListItem::new(l).hint(k)).collect();
    let d = ListDialog::new("Image actions", items, |app, a| {
        if let ListAction::Select(i) = a {
            match i {
                0 => transform(app, Transform::RotateRight),
                1 => transform(app, Transform::RotateLeft),
                2 => transform(app, Transform::Rotate180),
                3 => transform(app, Transform::FlipH),
                4 => transform(app, Transform::FlipV),
                5 => resize_dialog(app),
                6 => convert_dialog(app),
                7 => strip(app),
                _ => auto_orient(app),
            }
        }
    })
    .footer("Acts on marked images or the image under the cursor; Ctrl-X u undoes");
    app.push_dialog(Box::new(d));
}

#[cfg(test)]
mod tests;

//! Photo metadata (EXIF): a short summary and the orientation fix.

use exif::{In, Tag, Value};
use image::DynamicImage;

/// Summary for display plus the EXIF orientation (1 = normal).
#[derive(Debug, Default, Clone)]
pub struct Info {
    pub fields: Vec<(String, String)>,
    pub orientation: u32,
}

fn rational_deg(v: &Value) -> Option<f64> {
    if let Value::Rational(r) = v {
        let part = |i: usize| r.get(i).map(|x| x.to_f64()).filter(|x| x.is_finite());
        let d = part(0)?;
        return Some(d + part(1).unwrap_or(0.0) / 60.0 + part(2).unwrap_or(0.0) / 3600.0);
    }
    None
}

/// Read EXIF from JPEG / TIFF / PNG / WebP / HEIF bytes. None if there is no EXIF.
pub fn read(data: &[u8]) -> Option<Info> {
    let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(data)).ok()?;
    let get = |tag: Tag| exif.get_field(tag, In::PRIMARY);
    let show = |tag: Tag| {
        get(tag).map(|f| match &f.value {
            // ASCII fields may hold several NUL-separated strings; keep the non-empty ones.
            Value::Ascii(parts) => parts
                .iter()
                .map(|p| String::from_utf8_lossy(p).trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            _ => f.display_value().with_unit(&exif).to_string().trim_matches('"').trim().to_string(),
        })
    };
    let mut fields = Vec::new();
    let mut push = |label: &str, val: Option<String>| {
        if let Some(v) = val.filter(|v| !v.is_empty()) {
            fields.push((label.to_string(), v));
        }
    };
    let camera = match (show(Tag::Make), show(Tag::Model)) {
        (Some(make), Some(model)) if model.starts_with(&make) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (a, b) => a.or(b),
    };
    push("Camera", camera);
    push("Lens", show(Tag::LensModel));
    push("Taken", show(Tag::DateTimeOriginal).or_else(|| show(Tag::DateTime)));
    push("Exposure", show(Tag::ExposureTime));
    push("Aperture", show(Tag::FNumber));
    push("ISO", show(Tag::PhotographicSensitivity));
    push("Focal length", show(Tag::FocalLength));
    let dims = match (get(Tag::PixelXDimension), get(Tag::PixelYDimension)) {
        (Some(x), Some(y)) => match (x.value.get_uint(0), y.value.get_uint(0)) {
            (Some(x), Some(y)) => Some(format!("{x}x{y}")),
            _ => None,
        },
        _ => None,
    };
    push("Dimensions", dims);
    let lat = get(Tag::GPSLatitude).and_then(|f| rational_deg(&f.value));
    let lon = get(Tag::GPSLongitude).and_then(|f| rational_deg(&f.value));
    if let (Some(lat), Some(lon)) = (lat, lon) {
        let neg = |tag: Tag, n: &str| show(tag).is_some_and(|r| r.starts_with(n));
        let lat = if neg(Tag::GPSLatitudeRef, "S") { -lat } else { lat };
        let lon = if neg(Tag::GPSLongitudeRef, "W") { -lon } else { lon };
        push("GPS", Some(format!("{lat:.5}, {lon:.5}")));
    }
    let orientation =
        get(Tag::Orientation).and_then(|f| f.value.get_uint(0)).filter(|o| (1..=8).contains(o)).unwrap_or(1);
    Some(Info { fields, orientation })
}

/// Rotate / flip an image according to the EXIF orientation value.
pub fn apply_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal little-endian TIFF with Make, Model and Orientation = 6.
    fn tiff() -> Vec<u8> {
        let mut d = b"II*\0".to_vec();
        d.extend_from_slice(&8u32.to_le_bytes());
        let entries: u16 = 3;
        d.extend_from_slice(&entries.to_le_bytes());
        let data_off = 8 + 2 + 12 * 3 + 4;
        let entry = |tag: u16, typ: u16, count: u32, val: u32| {
            let mut e = Vec::new();
            e.extend_from_slice(&tag.to_le_bytes());
            e.extend_from_slice(&typ.to_le_bytes());
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&val.to_le_bytes());
            e
        };
        d.extend(entry(0x010f, 2, 6, data_off as u32));
        d.extend(entry(0x0110, 2, 5, data_off as u32 + 6));
        d.extend(entry(0x0112, 3, 1, 6));
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(b"Canon\0");
        d.extend_from_slice(b"EOS\0\0");
        d
    }

    #[test]
    fn reads_fields_and_orientation() {
        let info = read(&tiff()).expect("exif");
        assert_eq!(info.orientation, 6);
        let cam = info.fields.iter().find(|(k, _)| k == "Camera").map(|(_, v)| v.clone());
        assert_eq!(cam.as_deref(), Some("Canon EOS"));
        assert!(read(b"not an image").is_none());
    }

    #[test]
    fn orientation_rotates() {
        let img = DynamicImage::new_rgb8(4, 2);
        assert_eq!(apply_orientation(img.clone(), 6).width(), 2);
        assert_eq!(apply_orientation(img.clone(), 3).width(), 4);
        assert_eq!(apply_orientation(img, 99).width(), 4);
    }
}

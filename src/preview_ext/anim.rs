//! Animated GIF / APNG / WebP frame decoding.

use image::{AnimationDecoder, Frames, RgbaImage};
use std::io::Cursor;
use std::time::Duration;

/// Frames decoded at most.
pub const MAX_FRAMES: usize = 200;
/// Decoded RGBA bytes processed at most across all frames.
pub const MAX_DECODED_BYTES: usize = 256 << 20;

/// Largest canvas accepted (pixels). Decoders allocate the full canvas per frame,
/// so a crafted header must be rejected before decoding (found by fuzzing).
pub const MAX_CANVAS_PIXELS: u64 = 16 << 20;

/// Check the canvas and install allocation limits (frames may declare their own,
/// larger sizes; the decoders honor these limits per frame).
fn canvas_ok(d: &mut impl image::ImageDecoder) -> bool {
    let (w, h) = d.dimensions();
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_CANVAS_PIXELS {
        return false;
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 << 20);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    d.set_limits(limits).is_ok()
}

fn frames_of(data: &[u8]) -> Option<Frames<'_>> {
    let fmt = image::guess_format(data).ok()?;
    match fmt {
        image::ImageFormat::Gif => {
            let mut d = image::codecs::gif::GifDecoder::new(Cursor::new(data)).ok()?;
            canvas_ok(&mut d).then(|| d.into_frames())
        }
        image::ImageFormat::Png => {
            let mut d = image::codecs::png::PngDecoder::new(Cursor::new(data)).ok()?;
            if !canvas_ok(&mut d) || !d.is_apng().ok()? {
                return None;
            }
            d.apng().ok().map(|a| a.into_frames())
        }
        image::ImageFormat::WebP => {
            let mut d = image::codecs::webp::WebPDecoder::new(Cursor::new(data)).ok()?;
            if !canvas_ok(&mut d) || !d.has_animation() {
                return None;
            }
            Some(d.into_frames())
        }
        _ => None,
    }
}

/// Frame delay with the browser convention: very short delays mean 100 ms.
fn delay(f: &image::Frame) -> Duration {
    let (n, d) = f.delay().numer_denom_ms();
    let ms = n.checked_div(d).unwrap_or(100);
    Duration::from_millis(if ms < 20 { 100 } else { ms.min(10_000) as u64 })
}

/// Call `f` for each frame of an animated image (stops when `f` returns false or a
/// limit is hit). Returns the number of frames delivered, or None if the data is not
/// an animation (single-frame images return Some(1) only for GIF).
pub fn for_each_frame(data: &[u8], mut f: impl FnMut(RgbaImage, Duration) -> bool) -> Option<usize> {
    let frames = frames_of(data)?;
    let mut n = 0;
    let mut bytes = 0usize;
    for fr in frames.take(MAX_FRAMES) {
        let Ok(fr) = fr else { break };
        let d = delay(&fr);
        let buf = fr.into_buffer();
        bytes = bytes.saturating_add(buf.as_raw().len());
        if bytes > MAX_DECODED_BYTES {
            break;
        }
        n += 1;
        if !f(buf, d) {
            break;
        }
    }
    Some(n)
}

/// True if the bytes hold at least two frames.
pub fn is_animated(data: &[u8]) -> bool {
    let mut count = 0;
    for_each_frame(data, |_, _| {
        count += 1;
        count < 2
    });
    count >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba};

    pub(crate) fn sample_gif(frames: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..frames {
                let img = RgbaImage::from_pixel(8, 6, Rgba([(i * 60) as u8, 0, 0, 255]));
                enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(50, 1))).unwrap();
            }
        }
        out
    }

    #[test]
    fn decodes_gif_frames_and_delays() {
        let data = sample_gif(3);
        let mut got = Vec::new();
        assert_eq!(
            for_each_frame(&data, |img, d| {
                got.push((img.dimensions(), d));
                true
            }),
            Some(3)
        );
        assert_eq!(got[0], ((8, 6), Duration::from_millis(50)));
        assert!(is_animated(&data));
        assert!(!is_animated(&sample_gif(1)));
        assert!(for_each_frame(b"nope", |_, _| true).is_none());
    }
}

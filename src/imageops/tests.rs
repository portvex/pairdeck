use super::*;
use image::{GenericImageView, Rgba, RgbaImage};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// 2x1 image: red on the left, blue on the right.
fn two_px() -> DynamicImage {
    let mut i = RgbaImage::new(2, 1);
    i.put_pixel(0, 0, Rgba(RED));
    i.put_pixel(1, 0, Rgba(BLUE));
    DynamicImage::ImageRgba8(i)
}

fn png(img: &DynamicImage) -> Vec<u8> {
    let mut v = Vec::new();
    img.write_to(&mut Cursor::new(&mut v), ImageFormat::Png).unwrap();
    v
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    // Left half white, right half black: easy to check orientation after decoding.
    let img = image::RgbImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgb([255; 3]) } else { image::Rgb([0; 3]) });
    let mut v = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut v, 95)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    v
}

fn spec(action: Action) -> Spec {
    Spec { action, output: Output::Overwrite, jpeg_quality: 90, lossless_jpeg_rotate: true }
}

fn decode_all(data: &[u8]) -> (DynamicImage, Orientation, Option<Vec<u8>>) {
    let mut dec = ImageReader::new(Cursor::new(data)).with_guessed_format().unwrap().into_decoder().unwrap();
    let o = dec.orientation().unwrap();
    let exif = dec.exif_metadata().unwrap();
    (DynamicImage::from_decoder(dec).unwrap(), o, exif)
}

fn px(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    img.get_pixel(x, y).0
}

#[test]
fn compose_matches_sequential_application() {
    for a in ALL_ORIENTATIONS {
        for b in ALL_ORIENTATIONS {
            let c = compose(a, b);
            let mut seq = probe();
            seq.apply_orientation(a);
            seq.apply_orientation(b);
            let mut one = probe();
            one.apply_orientation(c);
            assert_eq!(seq, one, "{a:?} then {b:?}");
        }
    }
    assert_eq!(compose(Orientation::Rotate90, Orientation::Rotate90), Orientation::Rotate180);
    assert_eq!(compose(Orientation::Rotate270, Orientation::Rotate90), Orientation::NoTransforms);
}

#[test]
fn rotate_and_flip_pixels() {
    let src = png(&two_px());
    let run = |t| decode_all(&process(&src, "a.png", &spec(Action::Transform(t))).unwrap().data).0;
    let r = run(Transform::RotateRight);
    assert_eq!(r.dimensions(), (1, 2));
    assert_eq!((px(&r, 0, 0), px(&r, 0, 1)), (RED, BLUE), "rotate right: left pixel goes to the top");
    let l = run(Transform::RotateLeft);
    assert_eq!((px(&l, 0, 0), px(&l, 0, 1)), (BLUE, RED));
    let h = run(Transform::FlipH);
    assert_eq!((px(&h, 0, 0), px(&h, 1, 0)), (BLUE, RED));
    let v = run(Transform::FlipV);
    assert_eq!((px(&v, 0, 0), px(&v, 1, 0)), (RED, BLUE), "1 px tall: vertical flip changes nothing");
    let r180 = run(Transform::Rotate180);
    assert_eq!((px(&r180, 0, 0), px(&r180, 1, 0)), (BLUE, RED));
}

#[test]
fn lossless_jpeg_rotation_only_touches_exif() {
    let orig = jpeg(16, 8);
    let out = process(&orig, "p.jpg", &spec(Action::Transform(Transform::RotateRight))).unwrap();
    assert!(out.lossless);
    assert_eq!((out.width, out.height), (8, 16));
    // A 36 byte APP1 was inserted after the JFIF APP0; everything else is identical.
    assert_eq!(out.data.len(), orig.len() + 36);
    assert_eq!(&out.data[out.data.len() - (orig.len() - 20)..], &orig[20..], "compressed data unchanged");
    let (img, o, _) = decode_all(&out.data);
    assert_eq!(o, Orientation::Rotate90);
    assert_eq!(img.dimensions(), (16, 8), "pixels themselves were not touched");
    // Rotating again patches the existing tag in place.
    let again = process(&out.data, "p.jpg", &spec(Action::Transform(Transform::RotateRight))).unwrap();
    assert_eq!(again.data.len(), out.data.len());
    assert_eq!(decode_all(&again.data).1, Orientation::Rotate180);
    let back = process(&again.data, "p.jpg", &spec(Action::Transform(Transform::Rotate180))).unwrap();
    assert_eq!(decode_all(&back.data).1, Orientation::NoTransforms);
}

#[test]
fn reencoded_jpeg_rotation_bakes_pixels() {
    let mut s = spec(Action::Transform(Transform::RotateRight));
    s.lossless_jpeg_rotate = false;
    let out = process(&jpeg(16, 8), "p.jpg", &s).unwrap();
    assert!(!out.lossless);
    let (img, o, _) = decode_all(&out.data);
    assert_eq!(img.dimensions(), (8, 16));
    assert_eq!(o, Orientation::NoTransforms);
    // The white (left) half is now on top.
    assert!(px(&img, 4, 2)[0] > 200 && px(&img, 4, 13)[0] < 60);
}

#[test]
fn orientation_tag_parsing_both_endians() {
    // Little-endian TIFF with two entries; orientation is the second one.
    let mut t = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    t.extend_from_slice(&[2, 0]);
    t.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0]); // Make (ASCII)
    t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]); // Orientation = 6
    t.extend_from_slice(&[0, 0, 0, 0]);
    let (off, be) = find_orientation_tag(&t).unwrap();
    assert!(!be);
    assert_eq!(t[off], 6);
    assert_eq!(find_orientation_tag(&minimal_exif_app1(3)[10..]), Some((18, true)));
    assert!(find_orientation_tag(b"junk").is_none());
    assert!(find_orientation_tag(b"MM\x00\x2a\xff\xff\xff\xff").is_none(), "bad IFD offset");
}

/// A JPEG whose EXIF says "rotate 90 to display".
fn jpeg_oriented() -> Vec<u8> {
    set_jpeg_orientation(&jpeg(16, 8), Orientation::Rotate90).unwrap()
}

#[test]
fn auto_orient_and_strip_bake_orientation() {
    let src = jpeg_oriented();
    let a = process(&src, "o.jpg", &spec(Action::AutoOrient)).unwrap();
    let (img, o, _) = decode_all(&a.data);
    assert_eq!((img.dimensions(), o), ((8, 16), Orientation::NoTransforms));
    let s = process(&src, "o.jpg", &spec(Action::Strip)).unwrap();
    let (img, o, exif) = decode_all(&s.data);
    assert_eq!((img.dimensions(), o), ((8, 16), Orientation::NoTransforms));
    assert!(exif.is_none(), "metadata stripped");
    // Already upright, no metadata: nothing to do.
    assert!(process(&png(&two_px()), "a.png", &spec(Action::AutoOrient)).unwrap().unchanged);
    assert!(process(&png(&two_px()), "a.png", &spec(Action::Strip)).unwrap().unchanged);
}

#[test]
fn lossless_rotation_adds_tag_to_existing_exif() {
    // EXIF without an orientation tag (common for edited images): the tag is added
    // losslessly and the other EXIF data survives.
    let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08\x00\x02".to_vec();
    tiff.extend_from_slice(&[0x01, 0x0f, 0, 2, 0, 0, 0, 4, b'A', b'C', b'M', 0]); // Make = "ACM"
    tiff.extend_from_slice(&[0x01, 0x31, 0, 2, 0, 0, 0, 4, b'S', b'W', b'1', 0]); // Software = "SW1"
    tiff.extend_from_slice(&[0, 0, 0, 0]);
    let img = image::RgbImage::from_pixel(16, 8, image::Rgb([90, 90, 90]));
    let mut src = Vec::new();
    let mut e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 95);
    e.set_exif_metadata(tiff.clone()).unwrap();
    e.write_image(img.as_raw(), 16, 8, ExtendedColorType::Rgb8).unwrap();
    let out = process(&src, "e.jpg", &spec(Action::Transform(Transform::RotateRight))).unwrap();
    assert!(out.lossless, "tag added without re-encoding");
    let (img, o, exif) = decode_all(&out.data);
    assert_eq!((img.dimensions(), o), ((16, 8), Orientation::Rotate90));
    let exif = exif.unwrap();
    assert!(exif.windows(3).any(|w| w == b"ACM") && exif.windows(3).any(|w| w == b"SW1"), "EXIF kept");
    // The new IFD is sorted: Make (0x010f), Orientation (0x0112), Software (0x0131).
    let ifd = u32::from_be_bytes([exif[4], exif[5], exif[6], exif[7]]) as usize;
    let tag = |i: usize| u16::from_be_bytes([exif[ifd + 2 + 12 * i], exif[ifd + 3 + 12 * i]]);
    assert_eq!((tag(0), tag(1), tag(2)), (0x010f, 0x0112, 0x0131));
    // Little-endian blocks work too, and a block that has the tag is left alone.
    let le = b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let added = add_orientation_tag(le, 8).unwrap();
    assert_eq!(find_orientation_tag(&added).map(|(off, be)| (added[off], be)), Some((8, false)));
    assert!(add_orientation_tag(&added, 3).is_none());
}

#[test]
fn reencode_keeps_other_exif() {
    let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08\x00\x01".to_vec();
    tiff.extend_from_slice(&[0x01, 0x0f, 0, 2, 0, 0, 0, 4, b'A', b'C', b'M', 0]);
    tiff.extend_from_slice(&[0, 0, 0, 0]);
    let img = image::RgbImage::from_pixel(16, 8, image::Rgb([90, 90, 90]));
    let mut src = Vec::new();
    let mut e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 95);
    e.set_exif_metadata(tiff).unwrap();
    e.write_image(img.as_raw(), 16, 8, ExtendedColorType::Rgb8).unwrap();
    let mut s = spec(Action::Transform(Transform::RotateRight));
    s.lossless_jpeg_rotate = false;
    let out = process(&src, "e.jpg", &s).unwrap();
    assert!(!out.lossless);
    let (img, o, exif) = decode_all(&out.data);
    assert_eq!((img.dimensions(), o), ((8, 16), Orientation::NoTransforms));
    assert!(exif.unwrap().windows(3).any(|w| w == b"ACM"), "EXIF kept when re-encoding");
}

#[test]
fn sizes() {
    let o = |mode, only_shrink| ResizeOpts { mode, only_shrink, filter: FilterType::Triangle };
    assert_eq!(target_size(4000, 3000, &o(ResizeMode::Fit { w: 1920, h: 1080 }, true)).unwrap(), (1440, 1080));
    assert_eq!(target_size(4000, 3000, &o(ResizeMode::Fit { w: 1000, h: 0 }, true)).unwrap(), (1000, 750));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Fit { w: 1920, h: 1080 }, true)).unwrap(), (800, 600));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Fit { w: 1600, h: 0 }, false)).unwrap(), (1600, 1200));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Percent(50.0), false)).unwrap(), (400, 300));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Exact { w: 100, h: 100 }, false)).unwrap(), (100, 100));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Exact { w: 0, h: 300 }, false)).unwrap(), (400, 300));
    assert_eq!(target_size(800, 600, &o(ResizeMode::Fill { w: 200, h: 200 }, false)).unwrap(), (200, 200));
    assert!(target_size(8, 6, &o(ResizeMode::Fit { w: 0, h: 0 }, false)).is_err());
    assert!(target_size(8, 6, &o(ResizeMode::Fill { w: 10, h: 0 }, false)).is_err());
    assert!(target_size(8, 6, &o(ResizeMode::Percent(0.0), false)).is_err());
    assert_eq!(fill_crop(800, 600, 200, 200), (100, 0, 600, 600));
    assert_eq!(fill_crop(600, 800, 400, 200), (0, 250, 600, 300));
}

#[test]
fn resize_and_fill_pixels() {
    let src = png(&DynamicImage::ImageRgba8(RgbaImage::from_pixel(40, 20, Rgba(RED))));
    let opts = ResizeOpts { mode: ResizeMode::Fill { w: 10, h: 10 }, only_shrink: false, filter: FilterType::Nearest };
    let out = process(&src, "r.png", &spec(Action::Resize(opts))).unwrap();
    assert_eq!((out.width, out.height), (10, 10));
    assert_eq!(decode_all(&out.data).0.dimensions(), (10, 10));
    let same = ResizeOpts { mode: ResizeMode::Fit { w: 100, h: 100 }, only_shrink: true, filter: FilterType::Nearest };
    assert!(process(&src, "r.png", &spec(Action::Resize(same))).unwrap().unchanged);
}

#[test]
fn naming() {
    let src = Path::new("/p/photo.jpg");
    assert_eq!(output_path(src, &Output::Overwrite, None, (1, 1)), PathBuf::from("/p/photo.jpg"));
    assert_eq!(output_path(src, &Output::Overwrite, Some("png"), (1, 1)), PathBuf::from("/p/photo.png"));
    assert_eq!(
        output_path(src, &Output::Suffix("_{w}x{h}".into()), None, (1920, 1080)),
        PathBuf::from("/p/photo_1920x1080.jpg")
    );
    assert_eq!(output_path(src, &Output::Suffix(String::new()), None, (1, 1)), PathBuf::from("/p/photo_edited.jpg"));
    assert_eq!(output_path(src, &Output::Dir("out".into()), Some("webp"), (1, 1)), PathBuf::from("/p/out/photo.webp"));
    assert_eq!(output_path(src, &Output::Dir("/tmp/x".into()), None, (1, 1)), PathBuf::from("/tmp/x/photo.jpg"));
    assert_eq!(output_path(src, &Output::NewExtension, Some("png"), (1, 1)), PathBuf::from("/p/photo.png"));
    assert_eq!(output_path(src, &Output::NewExtension, None, (1, 1)), PathBuf::from("/p/photo_converted.jpg"));
    let taken = [PathBuf::from("/p/a.png"), PathBuf::from("/p/a (1).png")];
    assert_eq!(unique_path(Path::new("/p/a.png"), |p| taken.iter().any(|t| t == p)), PathBuf::from("/p/a (2).png"));
}

#[test]
fn convert_to_every_format() {
    let mut i = RgbaImage::from_pixel(300, 20, Rgba([0, 0, 0, 0]));
    i.put_pixel(0, 0, Rgba(RED));
    let src = png(&DynamicImage::ImageRgba8(i));
    for f in OutFormat::ALL {
        let s = spec(Action::Convert { format: *f, svg_size: 64 });
        let out = process(&src, "c.png", &s).unwrap_or_else(|e| panic!("{f:?}: {e:#}"));
        if *f == OutFormat::Png {
            assert!(out.unchanged, "same format: nothing to do");
            continue;
        }
        let (img, _, _) = decode_all(&out.data);
        assert_eq!(out.ext, Some(f.ext()));
        if *f == OutFormat::Ico {
            assert_eq!(img.dimensions(), (256, 17), "ICO capped at 256");
        } else {
            assert_eq!(img.dimensions(), (300, 20), "{f:?}");
        }
        if *f == OutFormat::Jpeg {
            assert!(px(&img, 150, 10)[0] > 240, "transparent areas become white in JPEG");
        }
    }
}

#[test]
fn svg_convert_and_refusals() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
    let out = process(svg, "v.svg", &spec(Action::Convert { format: OutFormat::Png, svg_size: 200 })).unwrap();
    assert_eq!((out.width, out.height, out.ext), (200, 100, Some("png")));
    assert!(process(svg, "v.svg", &spec(Action::Transform(Transform::RotateRight))).is_err());
    assert!(process(b"just text", "t.png", &spec(Action::AutoOrient)).is_err());
    // Animated GIFs are refused rather than silently flattened.
    let mut gif = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
        for c in [RED, BLUE] {
            let f = image::Frame::new(RgbaImage::from_pixel(4, 4, Rgba(c)));
            enc.encode_frame(f).unwrap();
        }
    }
    let e = process(&gif, "a.gif", &spec(Action::Transform(Transform::FlipH))).unwrap_err();
    assert!(format!("{e:#}").contains("animated"));
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pairdeck-imgops-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn file_jobs_write_atomically_and_undo() {
    let d = tempdir("jobs");
    let vfs = VfsHandle::local();
    let a = d.join("a.png");
    let orig = png(&two_px());
    std::fs::write(&a, &orig).unwrap();

    // Rotate in place: backup recorded, no temp files left behind.
    let r = process_file(&vfs, &a, &spec(Action::Transform(Transform::RotateRight))).unwrap();
    assert!(r.created.is_none());
    let (target, bak) = r.restore.clone().unwrap();
    assert_eq!(target, a);
    assert_eq!(image::open(&a).unwrap().dimensions(), (1, 2));
    let leftovers: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("pairdeck-tmp"))
        .collect();
    assert!(leftovers.is_empty());
    undo_restore(&vfs, &[(target, bak)], &[]).unwrap();
    assert_eq!(std::fs::read(&a).unwrap(), orig, "undo restored the exact bytes");

    // Convert replacing the original: new file, original removed, undo reverses both.
    let mut s = spec(Action::Convert { format: OutFormat::Jpeg, svg_size: 64 });
    s.output = Output::Overwrite;
    let r = process_file(&vfs, &a, &s).unwrap();
    let jpg = d.join("a.jpg");
    assert_eq!(r.created.as_deref(), Some(jpg.as_path()));
    assert!(jpg.exists() && !a.exists());
    undo_restore(&vfs, &[r.restore.unwrap()], std::slice::from_ref(&jpg)).unwrap();
    assert!(a.exists() && !jpg.exists());

    // Suffix output into a new subdirectory keeps the original and never clobbers.
    let mut s = spec(Action::Transform(Transform::FlipH));
    s.output = Output::Dir("out".into());
    let r1 = process_file(&vfs, &a, &s).unwrap();
    let r2 = process_file(&vfs, &a, &s).unwrap();
    assert_eq!(r1.created.unwrap(), d.join("out/a.png"));
    assert_eq!(r2.created.unwrap(), d.join("out/a (1).png"));
    assert!(r1.restore.is_none());
    assert_eq!(std::fs::read(&a).unwrap(), orig);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn write_atomic_failure_leaves_target_alone() {
    let d = tempdir("atomic");
    let t = d.join("keep.png");
    std::fs::write(&t, b"old").unwrap();
    let mut v = crate::vfs::local::LocalFs;
    // Target inside a missing directory: the write fails and nothing is created.
    assert!(write_atomic(&mut v, &d.join("missing/x.png"), b"new", 0o644).is_err());
    write_atomic(&mut v, &t, b"new", 0o600).unwrap();
    assert_eq!(std::fs::read(&t).unwrap(), b"new");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&t).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&d);
}

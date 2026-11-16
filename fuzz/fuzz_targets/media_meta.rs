//! Photo EXIF, font files, executables and animated images from untrusted bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::preview_ext::{anim, binary, exif, font};

fuzz_target!(|input: (u8, &[u8])| {
    let (sel, data) = input;
    match sel % 4 {
        0 => {
            if let Some(info) = exif::read(data) {
                let img = image::DynamicImage::new_rgb8(3, 2);
                let _ = exif::apply_orientation(img, info.orientation);
            }
        }
        1 => {
            let _ = font::info(data);
            let _ = font::sample_sheet(data);
        }
        2 => {
            let rows = binary::summary(data);
            assert!(!rows.is_empty());
        }
        _ => {
            let mut n = 0;
            let _ = anim::for_each_frame(data, |_, _| {
                n += 1;
                n < 8
            });
        }
    }
});

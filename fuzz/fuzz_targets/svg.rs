//! Untrusted SVG / SVGZ files rendered in the preview and viewer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Small raster keeps each run fast; parsing and the render path are what matter.
    let _ = pairdeck::svg::rasterize(data, 96);
});

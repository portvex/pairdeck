//! Image actions through the real UI: keys, the Image actions list, dialogs,
//! background job, undo.

mod common;

use common::{Ui, workdir, write};
use crossterm::event::{KeyCode, KeyModifiers};
use image::{GenericImageView, Rgba, RgbaImage};
use std::path::{Path, PathBuf};

/// 4x2 PNG, red on the left half, blue on the right.
fn make_png(p: &Path) {
    let img = RgbaImage::from_fn(4, 2, |x, _| if x < 2 { Rgba([255, 0, 0, 255]) } else { Rgba([0, 0, 255, 255]) });
    img.save(p).unwrap();
}

fn dims(p: &Path) -> Option<(u32, u32)> {
    image::open(p).ok().map(|i| i.dimensions())
}

fn setup(name: &str) -> (PathBuf, PathBuf) {
    let root = workdir(name);
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    (a, b)
}

#[test]
fn rotate_with_alt_r_then_undo() {
    let (a, b) = setup("img-rotate");
    let pic = a.join("pic.png");
    make_png(&pic);
    let orig = std::fs::read(&pic).unwrap();
    let mut ui = Ui::new(&a, &b);
    ui.select("pic.png");
    ui.key_mod(KeyCode::Char('r'), KeyModifiers::ALT);
    ui.wait_for("rotated to 2x4", |_| dims(&pic) == Some((2, 4)));
    let img = image::open(&pic).unwrap();
    assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255], "left half is now on top");
    // Ctrl-X u restores the original bytes.
    ui.ctrl('x');
    ui.key(KeyCode::Char('u'));
    ui.wait_for("undo restored the original", |_| std::fs::read(&pic).ok().as_deref() == Some(orig.as_slice()));
}

#[test]
fn convert_through_image_actions_dialog() {
    let (a, b) = setup("img-convert");
    let pic = a.join("pic.png");
    make_png(&pic);
    let mut ui = Ui::new(&a, &b);
    ui.select("pic.png");
    ui.ctrl('x');
    ui.key(KeyCode::Char('g'));
    ui.wait_text("Image actions");
    ui.typ("convert");
    ui.key(KeyCode::Enter);
    ui.wait_text("Convert images");
    // Focus starts on the first input (JPEG quality); walk up the format list to JPEG.
    for _ in 0..7 {
        ui.key(KeyCode::Up);
    }
    ui.key(KeyCode::Char(' '));
    ui.key(KeyCode::Enter);
    let jpg = a.join("pic.jpg");
    ui.wait_for("pic.jpg written", |_| dims(&jpg) == Some((4, 2)));
    assert!(pic.exists(), "default output keeps the original");
    assert_eq!(image::ImageFormat::from_path(&jpg).unwrap(), image::ImageFormat::Jpeg);
    ui.wait_for("panel shows pic.jpg", |ui| ui.names().contains(&"pic.jpg".to_string()));
}

#[test]
fn resize_marked_images_with_suffix_and_skip_text() {
    let (a, b) = setup("img-resize");
    make_png(&a.join("one.png"));
    make_png(&a.join("two.png"));
    write(&a.join("notes.txt"), "not an image\n");
    let mut ui = Ui::new(&a, &b);
    for n in ["notes.txt", "one.png", "two.png"] {
        ui.select(n);
        ui.key(KeyCode::Insert);
    }
    ui.ctrl('x');
    ui.key(KeyCode::Char('g'));
    ui.wait_text("Image actions");
    ui.typ("resize");
    ui.key(KeyCode::Enter);
    ui.wait_text("Resize images");
    // Width field is focused: clear it (Ctrl-U) and type, then the height field.
    ui.ctrl('u');
    ui.typ("2");
    ui.key(KeyCode::Down);
    ui.ctrl('u');
    ui.typ("1");
    ui.key(KeyCode::Enter);
    for n in ["one_2x1.png", "two_2x1.png"] {
        let p = a.join(n);
        ui.wait_for(n, move |_| dims(&p) == Some((2, 1)));
    }
    assert_eq!(dims(&a.join("one.png")), Some((4, 2)), "originals untouched");
    assert!(!a.join("notes_2x1.txt").exists());
    ui.wait_text("non-image(s) skipped");
}

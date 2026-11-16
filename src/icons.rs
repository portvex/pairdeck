//! Nerd Font icons and file categories.

use crate::theme::Theme;
use crate::vfs::{Entry, EntryKind};
use ratatui::style::{Color, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Dir,
    Exec,
    Symlink,
    Broken,
    Archive,
    Image,
    Audio,
    Video,
    Document,
    Source,
    Hidden,
    Special,
    Other,
}

pub fn ext_of(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for multi in ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"] {
        if lower.ends_with(&format!(".{multi}")) {
            return multi.to_string();
        }
    }
    match lower.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => lower[i + 1..].to_string(),
    }
}

pub const ARCHIVE_EXTS: &[&str] = &[
    "zip", "jar", "war", "apk", "tar", "tgz", "tar.gz", "gz", "tbz", "tbz2", "tar.bz2", "bz2", "txz", "tar.xz", "xz",
    "7z", "rar", "zst", "tar.zst", "deb", "rpm", "iso", "dmg", "whl", "epub",
];
pub const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif", "ico", "svg", "svgz", "heic", "avif", "tga", "qoi",
    "pnm", "ppm", "pgm", "exr", "hdr",
];
pub const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma", "aiff"];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg"];
pub const DOC_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "xls", "xlsx", "ods", "ppt", "pptx", "odp", "md", "txt", "rst", "org", "tex",
    "csv",
];
pub const SOURCE_EXTS: &[&str] = &[
    "rs", "c", "h", "cpp", "hpp", "cc", "cxx", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "go", "java", "kt",
    "swift", "rb", "php", "lua", "sh", "bash", "zsh", "fish", "pl", "hs", "ml", "ex", "exs", "erl", "cs", "fs",
    "scala", "clj", "dart", "zig", "nim", "r", "jl", "vim", "el", "sql", "html", "htm", "css", "scss", "sass", "less",
    "vue", "svelte", "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf", "mk", "cmake", "gradle", "nix", "tf",
];

pub fn category(e: &Entry) -> Category {
    match &e.kind {
        EntryKind::Dir => return Category::Dir,
        EntryKind::Symlink { broken: true, .. } => return Category::Broken,
        EntryKind::Symlink { to_dir: true, .. } => return Category::Dir,
        EntryKind::Other => return Category::Special,
        _ => {}
    }
    let ext = ext_of(&e.name);
    let ext = ext.as_str();
    if ARCHIVE_EXTS.contains(&ext) {
        Category::Archive
    } else if IMAGE_EXTS.contains(&ext) {
        Category::Image
    } else if AUDIO_EXTS.contains(&ext) {
        Category::Audio
    } else if VIDEO_EXTS.contains(&ext) {
        Category::Video
    } else if DOC_EXTS.contains(&ext) {
        Category::Document
    } else if SOURCE_EXTS.contains(&ext) {
        Category::Source
    } else if e.is_exec() {
        Category::Exec
    } else if matches!(e.kind, EntryKind::Symlink { .. }) {
        Category::Symlink
    } else if e.name.starts_with('.') {
        Category::Hidden
    } else {
        Category::Other
    }
}

pub fn style_for(e: &Entry, theme: &Theme) -> Style {
    let base = match category(e) {
        Category::Dir => theme.dir,
        Category::Exec => theme.exec,
        Category::Symlink => theme.symlink,
        Category::Broken => theme.broken,
        Category::Archive => theme.archive,
        Category::Image => theme.image,
        Category::Audio | Category::Video => theme.media,
        Category::Document => theme.doc,
        Category::Source => theme.source,
        Category::Hidden => theme.hidden,
        Category::Special => theme.special,
        Category::Other => theme.panel,
    };
    if e.name.starts_with('.') && e.name != ".." && !matches!(category(e), Category::Dir) {
        return theme.hidden;
    }
    if matches!(e.kind, EntryKind::Symlink { broken: false, .. }) {
        return base.patch(Style::default().add_modifier(ratatui::style::Modifier::ITALIC));
    }
    base
}

/// Returns (glyph, optional icon color).
pub fn icon_for(e: &Entry) -> (&'static str, Option<Color>) {
    let lname = e.name.to_ascii_lowercase();
    if e.name == ".." {
        return ("\u{f062}", None);
    }
    match &e.kind {
        EntryKind::Dir | EntryKind::Symlink { to_dir: true, .. } => {
            let g = match lname.as_str() {
                ".git" => "\u{e5fb}",
                ".github" => "\u{e5fd}",
                ".config" | "config" | ".cargo" => "\u{e5fc}",
                "node_modules" => "\u{e5fa}",
                "desktop" => "\u{f108}",
                "downloads" => "\u{f019}",
                "documents" => "\u{f0c7}",
                "music" => "\u{f001}",
                "pictures" | "images" => "\u{f03e}",
                "videos" | "movies" => "\u{f03d}",
                ".ssh" => "\u{f084}",
                "src" => "\u{f121}",
                "test" | "tests" => "\u{f0668}",
                _ if matches!(e.kind, EntryKind::Symlink { .. }) => "\u{f0c1}",
                _ => "\u{f07b}",
            };
            return (g, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        }
        EntryKind::Symlink { broken: true, .. } => return ("\u{f127}", Some(Color::Rgb(0xf3, 0x8b, 0xa8))),
        EntryKind::Other => return ("\u{f2db}", None),
        _ => {}
    }
    let by_name = match lname.as_str() {
        "cargo.toml" | "cargo.lock" => Some(("\u{e68b}", 0xdea584)),
        "makefile" | "gnumakefile" | "cmakelists.txt" => Some(("\u{e673}", 0x6d8086)),
        "dockerfile" | "docker-compose.yml" | "compose.yaml" | "containerfile" => Some(("\u{e650}", 0x458ee6)),
        "license" | "license.md" | "license.txt" | "copying" => Some(("\u{f02d}", 0xd0bf41)),
        "readme" | "readme.md" | "readme.txt" => Some(("\u{f00ba}", 0x42a5f5)),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitconfig" => Some(("\u{f1d3}", 0xf14c28)),
        "package.json" | "package-lock.json" => Some(("\u{e71e}", 0xe8274b)),
        ".zshrc" | ".bashrc" | ".profile" | ".bash_profile" | ".zprofile" => Some(("\u{f489}", 0x89e051)),
        ".env" | ".envrc" => Some(("\u{f462}", 0xfaf743)),
        "go.mod" | "go.sum" => Some(("\u{e65e}", 0x00add8)),
        _ => None,
    };
    if let Some((g, c)) = by_name {
        return (g, Some(hex(c)));
    }
    let ext = ext_of(&e.name);
    let (g, c): (&str, u32) = match ext.as_str() {
        "rs" => ("\u{e68b}", 0xdea584),
        "py" | "pyi" | "pyc" => ("\u{e606}", 0xffbc03),
        "js" | "mjs" | "cjs" => ("\u{e74e}", 0xf1e05a),
        "ts" | "mts" | "cts" => ("\u{e628}", 0x519aba),
        "jsx" | "tsx" => ("\u{e7ba}", 0x20c2e3),
        "go" => ("\u{e65e}", 0x00add8),
        "c" | "h" => ("\u{e61e}", 0x599eff),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => ("\u{e61d}", 0xf34b7d),
        "cs" => ("\u{f031b}", 0x596706),
        "java" | "jar" | "class" => ("\u{e256}", 0xcc3e44),
        "kt" | "kts" => ("\u{e634}", 0x7f52ff),
        "swift" => ("\u{e755}", 0xe37933),
        "rb" => ("\u{e739}", 0x701516),
        "php" => ("\u{e73d}", 0xa074c4),
        "lua" => ("\u{e620}", 0x51a0cf),
        "hs" | "lhs" => ("\u{e777}", 0xa074c4),
        "ex" | "exs" => ("\u{e62d}", 0xa074c4),
        "ml" | "mli" => ("\u{e67a}", 0xe37933),
        "pl" | "pm" => ("\u{e67e}", 0x519aba),
        "r" => ("\u{e68a}", 0x2266ba),
        "zig" => ("\u{e6a9}", 0xf69a1b),
        "nix" => ("\u{f313}", 0x7ebae4),
        "dart" => ("\u{e798}", 0x03589c),
        "scala" => ("\u{e737}", 0xcc3e44),
        "vim" => ("\u{e7c5}", 0x019833),
        "sh" | "bash" | "zsh" | "fish" | "ksh" => ("\u{f489}", 0x89e051),
        "ps1" | "psm1" => ("\u{ebc7}", 0x4273ca),
        "html" | "htm" | "xhtml" => ("\u{f13b}", 0xe44d26),
        "css" => ("\u{e749}", 0x42a5f5),
        "scss" | "sass" => ("\u{e603}", 0xf55385),
        "vue" => ("\u{e6a0}", 0x8dc149),
        "svelte" => ("\u{e697}", 0xff3e00),
        "json" | "jsonc" | "json5" => ("\u{e60b}", 0xcbcb41),
        "yaml" | "yml" => ("\u{e6a8}", 0x6d8086),
        "toml" => ("\u{e6b2}", 0x9c4221),
        "xml" | "xsd" | "xsl" => ("\u{f05c0}", 0xe37933),
        "ini" | "cfg" | "conf" | "config" => ("\u{e615}", 0x6d8086),
        "md" | "markdown" | "mdx" => ("\u{f48a}", 0x519aba),
        "txt" | "log" => ("\u{f15c}", 0x89e051),
        "rst" | "org" | "adoc" => ("\u{f15c}", 0x89e051),
        "tex" | "bib" => ("\u{e69b}", 0x3d6117),
        "pdf" => ("\u{f1c1}", 0xb30b00),
        "doc" | "docx" | "odt" | "rtf" => ("\u{f1c2}", 0x185abd),
        "xls" | "xlsx" | "ods" | "csv" | "tsv" => ("\u{f1c3}", 0x207245),
        "ppt" | "pptx" | "odp" => ("\u{f1c4}", 0xcb4a32),
        "epub" | "mobi" | "djvu" => ("\u{e28b}", 0xeab16d),
        "sql" | "db" | "sqlite" | "sqlite3" => ("\u{f1c0}", 0xdad8d8),
        "lock" => ("\u{f023}", 0xbbbbbb),
        "diff" | "patch" => ("\u{f440}", 0x41535b),
        "ttf" | "otf" | "woff" | "woff2" => ("\u{f031}", 0xececec),
        "pem" | "crt" | "cer" | "key" | "pub" | "asc" | "gpg" | "sig" => ("\u{eb11}", 0xe8c547),
        "iso" | "img" | "dmg" | "vhd" | "vmdk" | "qcow2" => ("\u{e271}", 0xd0bec8),
        "exe" | "msi" | "dll" | "bat" | "cmd" => ("\u{f17a}", 0x00a4ef),
        "app" | "ipa" => ("\u{f179}", 0xa2aaad),
        "deb" | "rpm" | "apk" | "AppImage" => ("\u{f17c}", 0xfcc624),
        "o" | "a" | "so" | "dylib" | "bin" | "elf" | "wasm" => ("\u{eae8}", 0x9f0500),
        "srt" | "vtt" | "ass" | "sub" => ("\u{f0a16}", 0xffb713),
        "torrent" => ("\u{f01da}", 0x44cda8),
        _ => {
            if ARCHIVE_EXTS.contains(&ext.as_str()) {
                ("\u{f410}", 0xeca517)
            } else if IMAGE_EXTS.contains(&ext.as_str()) {
                ("\u{f1c5}", 0xa074c4)
            } else if AUDIO_EXTS.contains(&ext.as_str()) {
                ("\u{f001}", 0x00afff)
            } else if VIDEO_EXTS.contains(&ext.as_str()) {
                ("\u{f03d}", 0xfd971f)
            } else if e.is_exec() {
                ("\u{f489}", 0x89e051)
            } else if matches!(e.kind, EntryKind::Symlink { .. }) {
                ("\u{f481}", 0x94e2d5)
            } else {
                ("\u{f15b}", 0x9ca0b0)
            }
        }
    };
    (g, Some(hex(c)))
}

fn hex(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Plain ASCII markers used when Nerd Fonts are disabled (MC style).
pub fn ascii_marker(e: &Entry) -> char {
    match &e.kind {
        EntryKind::Dir => '/',
        EntryKind::Symlink { broken: true, .. } => '!',
        EntryKind::Symlink { to_dir: true, .. } => '~',
        EntryKind::Symlink { .. } => '@',
        EntryKind::Other => '=',
        _ if e.is_exec() => '*',
        _ => ' ',
    }
}

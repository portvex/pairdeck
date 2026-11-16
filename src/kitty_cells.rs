//! Complete kitty graphics Unicode placeholders.
//!
//! ratatui-image writes the row / column / image-id diacritics only on the first
//! placeholder cell of each image row and leaves the following cells as a bare
//! U+10EEEE, relying on the kitty rule that a bare placeholder continues the
//! previous one. kitty and Ghostty implement that rule, but terminal
//! multiplexers that re-render placeholders themselves (tuidesk, for example)
//! do not, so only the first column of every image appeared there. Writing the
//! full diacritics on every cell is equally valid for every terminal.
//!
//! The diacritics table is kitty's rowcolumn-diacritics list, as shipped in
//! ratatui-image (MIT / Apache-2.0).

use ratatui::{buffer::Buffer, layout::Rect};

pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Diacritics encoding the numbers 0..=296 (row, column, id high byte).
pub static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

fn index_of(d: char) -> Option<usize> {
    DIACRITICS.iter().position(|&c| c == d)
}

/// Give every bare placeholder cell in `area` explicit row, column and id
/// diacritics derived from the first placeholder cell of its row. Cells that
/// already carry diacritics and non-placeholder cells are left alone.
pub fn complete_placeholders(buf: &mut Buffer, area: Rect) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        // (row diacritic, column index of the anchor, id high-byte diacritic, anchor x)
        let mut anchor: Option<(char, usize, Option<char>, u16)> = None;
        for x in area.left()..area.right() {
            let sym = buf[(x, y)].symbol().to_string();
            let Some(pos) = sym.find(PLACEHOLDER) else {
                anchor = None;
                continue;
            };
            let marks: Vec<char> = sym[pos + PLACEHOLDER.len_utf8()..].chars().collect();
            if marks.is_empty() {
                // Bare placeholder: continue the anchor of this row.
                if let Some((row, col0, msb, ax)) = anchor {
                    let col = col0 + usize::from(x - ax);
                    let Some(&cd) = DIACRITICS.get(col) else { continue };
                    let mut s = String::with_capacity(16);
                    s.push(PLACEHOLDER);
                    s.push(row);
                    s.push(cd);
                    if let Some(m) = msb {
                        s.push(m);
                    }
                    buf[(x, y)].set_symbol(&s);
                }
                continue;
            }
            // Explicit placeholder: becomes the anchor for the cells after it.
            let row = marks[0];
            let col0 = marks.get(1).and_then(|&c| index_of(c)).unwrap_or(0);
            anchor = Some((row, col0, marks.get(2).copied(), x));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::{
        FontSize, Image, Resize,
        picker::{Picker, ProtocolType},
    };

    fn decode(sym: &str) -> Option<(usize, usize)> {
        let pos = sym.find(PLACEHOLDER)?;
        let m: Vec<char> = sym[pos + PLACEHOLDER.len_utf8()..].chars().collect();
        Some((index_of(*m.first()?)?, index_of(*m.get(1)?)?))
    }

    /// A real ratatui-image kitty render: afterwards every placeholder cell has
    /// explicit diacritics that decode to its own row and column, and the id
    /// high byte of the anchor is repeated.
    #[test]
    #[allow(deprecated)]
    fn every_cell_gets_row_and_column() {
        let mut picker = Picker::from_fontsize(FontSize::new(8, 16));
        picker.set_protocol_type(ProtocolType::Kitty);
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(80, 64, image::Rgb([200, 30, 30])));
        let proto = picker.new_protocol(img, ratatui::layout::Size::new(10, 4), Resize::Fit(None)).unwrap();
        let mut term = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let r = Rect::new(3, 2, 10, 4);
        let frame = term
            .draw(|f| {
                f.render_widget(Image::new(&proto).allow_clipping(true), r);
                complete_placeholders(f.buffer_mut(), r);
            })
            .unwrap();
        let buf = frame.buffer;
        let mut seen = 0;
        let mut msb = None;
        for y in r.top()..r.bottom() {
            for x in r.left()..r.right() {
                let sym = buf[(x, y)].symbol();
                if !sym.contains(PLACEHOLDER) {
                    continue;
                }
                seen += 1;
                let (row, col) = decode(sym).unwrap_or_else(|| panic!("bare placeholder left at {x},{y}"));
                assert_eq!((row, col), ((y - r.y) as usize, (x - r.x) as usize), "cell {x},{y}");
                let m: Vec<char> = sym[sym.find(PLACEHOLDER).unwrap() + 4..].chars().collect();
                let this = m.get(2).copied();
                assert!(msb.is_none() || msb == Some(this), "id byte differs at {x},{y}");
                msb = Some(this);
            }
        }
        assert_eq!(seen, 40, "expected a 10x4 image");
    }

    /// Text cells and already explicit placeholders are untouched.
    #[test]
    fn leaves_other_cells_alone() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        buf[(0, 0)].set_symbol("a");
        let explicit = format!("{PLACEHOLDER}{}{}", DIACRITICS[0], DIACRITICS[3]);
        buf[(1, 0)].set_symbol(&explicit);
        buf[(2, 0)].set_symbol(&PLACEHOLDER.to_string());
        buf[(3, 0)].set_symbol("b");
        buf[(4, 0)].set_symbol(&PLACEHOLDER.to_string());
        complete_placeholders(&mut buf, Rect::new(0, 0, 6, 1));
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), explicit);
        assert_eq!(decode(buf[(2, 0)].symbol()), Some((0, 4)), "continues the anchor's column");
        assert_eq!(buf[(3, 0)].symbol(), "b");
        assert_eq!(buf[(4, 0)].symbol(), PLACEHOLDER.to_string(), "no anchor after text: left alone");
    }
}

//! CSV / TSV / PSV files as an aligned table.

use crate::theme::Theme;
use crate::util::{pad_left, pad_right, str_width, trunc_end};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Rows rendered at most (the preview pane is for a quick look).
pub const MAX_ROWS: usize = 5000;
/// Widest a column may get, in cells.
const MAX_COL: usize = 40;
/// Rows sampled for column widths and numeric detection.
const SAMPLE: usize = 300;
/// Columns kept at most (very wide exports).
const MAX_COLUMNS: usize = 200;

pub fn is_table_ext(ext: &str) -> bool {
    matches!(ext, "csv" | "tsv" | "tab" | "psv")
}

/// Pick the delimiter: by extension, else the candidate that splits the first
/// lines into the most consistent number of fields.
pub fn sniff_delimiter(text: &str, ext: &str) -> u8 {
    match ext {
        "tsv" | "tab" => return b'\t',
        "psv" => return b'|',
        _ => {}
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(20).collect();
    let mut best = (b',', 0usize);
    for d in *b",;\t|" {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(d as char).count()).collect();
        let Some(&first) = counts.first() else { continue };
        if first == 0 {
            continue;
        }
        let consistent = counts.iter().filter(|&&c| c == first).count();
        let score = consistent * 1000 + first.min(999);
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

/// Parsed table: rows of cells (the first row is treated as the header).
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub columns: usize,
    pub truncated: bool,
}

pub fn parse(text: &str, ext: &str) -> Option<Table> {
    let delim = sniff_delimiter(text, ext);
    let mut rdr =
        csv::ReaderBuilder::new().delimiter(delim).has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut rows = Vec::new();
    let mut truncated = false;
    for rec in rdr.records() {
        let Ok(rec) = rec else { break };
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        rows.push(rec.iter().take(MAX_COLUMNS).map(|f| f.replace(['\n', '\r', '\t'], " ")).collect::<Vec<_>>());
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if rows.is_empty() || columns == 0 {
        return None;
    }
    Some(Table { rows, columns, truncated })
}

fn is_number(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.replace(['_', ','], "").trim_start_matches(['+', '-']).parse::<f64>().is_ok()
}

/// Render as aligned lines. Lines may be wider than the pane; the preview
/// scrolls horizontally (Left/Right when the preview is focused).
pub fn render(table: &Table, t: &Theme) -> Vec<Line<'static>> {
    let n = table.columns;
    let mut widths = vec![1usize; n];
    let mut numeric = vec![true; n];
    for (ri, row) in table.rows.iter().take(SAMPLE).enumerate() {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(str_width(cell).min(MAX_COL));
            if ri > 0 && !cell.trim().is_empty() && !is_number(cell) {
                numeric[c] = false;
            }
        }
    }
    let sep_style = t.border;
    let mut out = Vec::with_capacity(table.rows.len() + 3);
    out.push(Line::styled(
        format!(
            "{} rows x {} columns{}",
            table.rows.len().saturating_sub(1),
            n,
            if table.truncated { format!(" (first {MAX_ROWS} shown)") } else { String::new() }
        ),
        t.panel.patch(t.hidden),
    ));
    for (ri, row) in table.rows.iter().enumerate() {
        let mut spans = Vec::with_capacity(n * 2);
        for c in 0..n {
            if c > 0 {
                spans.push(Span::styled(" \u{2502} ", sep_style));
            }
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            let txt = trunc_end(cell, widths[c]);
            let txt = if numeric[c] && ri > 0 { pad_left(&txt, widths[c]) } else { pad_right(&txt, widths[c]) };
            let st = if ri == 0 { t.header.add_modifier(Modifier::BOLD) } else { t.panel };
            spans.push(Span::styled(txt, st));
        }
        out.push(Line::from(spans));
        if ri == 0 {
            let rule: Vec<String> = widths.iter().map(|w| "\u{2500}".repeat(*w)).collect();
            out.push(Line::styled(rule.join("\u{2500}\u{253c}\u{2500}"), sep_style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_and_parses_quoted_fields() {
        let text = "name;age;city\n\"Doe; John\";42;Paris\nAnn;7;\"New\nYork\"\n";
        assert_eq!(sniff_delimiter(text, "csv"), b';');
        let t = parse(text, "csv").unwrap();
        assert_eq!(t.columns, 3);
        assert_eq!(t.rows[1][0], "Doe; John");
        assert_eq!(t.rows[2][2], "New York");
        assert_eq!(sniff_delimiter("a\tb\n1\t2\n", "tsv"), b'\t');
    }

    #[test]
    fn renders_aligned_with_numeric_columns_right() {
        let t = parse("item,qty\napple,3\nbanana,120\n", "csv").unwrap();
        let theme = Theme::by_name("dark");
        let lines: Vec<String> =
            render(&t, &theme).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert!(lines[0].starts_with("2 rows x 2 columns"));
        assert!(lines[3].ends_with("  3"), "numbers right aligned: {:?}", lines[3]);
        assert_eq!(str_width(&lines[3]), str_width(&lines[4]));
    }

    #[test]
    fn ragged_and_empty_input() {
        assert!(parse("", "csv").is_none());
        let t = parse("a,b,c\n1\n1,2,3,4\n", "csv").unwrap();
        assert_eq!(t.columns, 4);
        let _ = render(&t, &Theme::by_name("light"));
    }
}

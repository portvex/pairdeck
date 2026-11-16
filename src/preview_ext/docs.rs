//! Plain text from office documents and e-books (docx, odt, ods, odp, pptx,
//! xlsx, epub): unzip, pick the content parts, strip XML tags.

use anyhow::{Context, Result, bail};
use std::io::{Cursor, Read};

/// Bytes decompressed from one part at most.
const MAX_PART: u64 = 16 << 20;
/// Text returned at most.
pub const MAX_TEXT: usize = 2 << 20;
/// Parts read at most (slides, chapters).
const MAX_PARTS: usize = 500;

pub fn is_doc_ext(ext: &str) -> bool {
    matches!(ext, "docx" | "docm" | "odt" | "ods" | "odp" | "pptx" | "xlsx" | "xlsm" | "epub")
}

fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let n = ent.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Strip tags. `breaks` are tag names (without namespace prefix handling: give
/// them as they appear, e.g. "w:p") whose closing tag starts a new line; `tabs`
/// produce a tab. Script and style contents are dropped.
pub fn strip_xml(xml: &str, breaks: &[&str], tabs: &[&str]) -> String {
    let mut out = String::with_capacity(xml.len() / 3);
    let mut i = 0;
    let b = xml.as_bytes();
    let mut skip_until: Option<&str> = None;
    while i < b.len() && out.len() < MAX_TEXT {
        if b[i] == b'<' {
            let Some(end) = xml[i..].find('>') else { break };
            let tag = &xml[i + 1..i + end];
            i += end + 1;
            let closing = tag.starts_with('/');
            let name = tag.trim_start_matches('/').split([' ', '/', '\t', '\n']).next().unwrap_or("");
            let lname = name.to_ascii_lowercase();
            if let Some(stop) = skip_until {
                if closing && lname == stop {
                    skip_until = None;
                }
                continue;
            }
            if !closing && (lname == "script" || lname == "style") && !tag.ends_with('/') {
                skip_until = Some(if lname == "script" { "script" } else { "style" });
                continue;
            }
            let self_closing = tag.ends_with('/');
            if (closing || self_closing) && breaks.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                out.push('\n');
            } else if tabs.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                out.push('\t');
            }
            continue;
        }
        if skip_until.is_some() {
            i += 1;
            continue;
        }
        if b[i] == b'&'
            && let Some(semi) = xml[i..].find(';').filter(|&s| s <= 10)
            && let Some(c) = decode_entity(&xml[i + 1..i + semi])
        {
            out.push(c);
            i += semi + 1;
            continue;
        }
        let ch = xml[i..].chars().next().unwrap_or(' ');
        out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
        i += ch.len_utf8();
    }
    // Collapse runs of blank lines and trailing spaces.
    let mut clean = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        clean.push_str(l.trim_start_matches(' '));
        clean.push('\n');
    }
    clean
}

fn read_part(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.take(MAX_PART).read_to_string(&mut s).ok()?;
    Some(s)
}

fn numbered(names: &[String], prefix: &str, suffix: &str) -> Vec<String> {
    let mut v: Vec<(u32, String)> = names
        .iter()
        .filter_map(|n| {
            let num = n.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some((num.parse().ok()?, n.clone()))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, n)| n).take(MAX_PARTS).collect()
}

/// Extract readable text from a document.
pub fn extract_text(data: &[u8], ext: &str) -> Result<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(data)).context("not a valid document (zip container)")?;
    let names: Vec<String> = zip.file_names().take(20_000).filter_map(|r| r.ok().map(|c| c.into_owned())).collect();
    let mut out = String::new();
    match ext {
        "docx" | "docm" => {
            let xml = read_part(&mut zip, "word/document.xml").context("word/document.xml missing")?;
            out = strip_xml(&xml, &["w:p", "w:br", "w:tr"], &["w:tab", "w:tc"]);
        }
        "odt" | "ods" | "odp" => {
            let xml = read_part(&mut zip, "content.xml").context("content.xml missing")?;
            out = strip_xml(
                &xml,
                &["text:p", "text:h", "text:line-break", "table:table-row"],
                &["text:tab", "table:table-cell"],
            );
        }
        "pptx" => {
            for (i, part) in numbered(&names, "ppt/slides/slide", ".xml").iter().enumerate() {
                if let Some(xml) = read_part(&mut zip, part) {
                    out.push_str(&format!("--- Slide {} ---\n", i + 1));
                    out.push_str(&strip_xml(&xml, &["a:p", "a:br"], &[]));
                    out.push('\n');
                }
                if out.len() > MAX_TEXT {
                    break;
                }
            }
        }
        "xlsx" | "xlsm" => {
            if let Some(wb) = read_part(&mut zip, "xl/workbook.xml") {
                let sheets: Vec<String> = wb
                    .split("<sheet ")
                    .skip(1)
                    .filter_map(|s| s.split("name=\"").nth(1)?.split('"').next().map(String::from))
                    .collect();
                if !sheets.is_empty() {
                    out.push_str(&format!("Sheets: {}\n\n", sheets.join(", ")));
                }
            }
            if let Some(ss) = read_part(&mut zip, "xl/sharedStrings.xml") {
                out.push_str("Text cells:\n");
                out.push_str(&strip_xml(&ss, &["si"], &[]));
            }
        }
        "epub" => {
            let mut parts: Vec<&String> = names
                .iter()
                .filter(|n| {
                    let l = n.to_ascii_lowercase();
                    (l.ends_with(".xhtml") || l.ends_with(".html") || l.ends_with(".htm")) && !l.contains("nav")
                })
                .collect();
            parts.sort();
            for part in parts.into_iter().take(MAX_PARTS) {
                if let Some(xml) = read_part(&mut zip, part) {
                    out.push_str(&strip_xml(
                        &xml,
                        &["p", "br", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "tr", "title"],
                        &["td"],
                    ));
                    out.push('\n');
                }
                if out.len() > MAX_TEXT {
                    break;
                }
            }
        }
        _ => bail!("unsupported document type"),
    }
    out.truncate(floor_char_boundary(&out, MAX_TEXT));
    Ok(out)
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (n, c) in files {
                w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(c.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn strips_tags_and_entities() {
        let s =
            strip_xml("<p>Fish &amp; chips&#33;</p><p>2 &lt; 3<br/>next</p><script>x()</script>", &["p", "br"], &[]);
        assert_eq!(s, "Fish & chips!\n2 < 3\nnext\n");
    }

    #[test]
    fn docx_odt_pptx() {
        let docx = zip_with(&[(
            "word/document.xml",
            "<w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:p><w:t>World</w:t></w:p></w:body>",
        )]);
        assert_eq!(extract_text(&docx, "docx").unwrap(), "Hello\nWorld\n");
        let odt = zip_with(&[("content.xml", "<text:h>Title</text:h><text:p>Body</text:p>")]);
        assert_eq!(extract_text(&odt, "odt").unwrap(), "Title\nBody\n");
        let pptx = zip_with(&[
            ("ppt/slides/slide2.xml", "<a:p><a:t>Two</a:t></a:p>"),
            ("ppt/slides/slide1.xml", "<a:p><a:t>One</a:t></a:p>"),
        ]);
        let t = extract_text(&pptx, "pptx").unwrap();
        assert!(t.find("One").unwrap() < t.find("Two").unwrap());
        assert!(extract_text(b"garbage", "docx").is_err());
    }
}

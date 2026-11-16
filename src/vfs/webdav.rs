//! WebDAV backend (Nextcloud, ownCloud, Apache mod_dav, nginx, rclone, ...).
//!
//! URLs: `dav://` and `webdav://` use http, `davs://` and `webdavs://` use https;
//! `http(s)://` is accepted too. Credentials can be given in the URL
//! (`davs://user:pass@host/path`) or in the Connect dialog (HTTP Basic auth).

use super::http::{self, UploadOnClose};
use super::{Entry, EntryKind, Location, Vfs, VfsHandle, normalize};
use anyhow::{Context, Result, anyhow, bail};
use quick_xml::events::Event;
use std::io::Read;
use std::path::{Path, PathBuf};

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

/// Is this a WebDAV URL we handle?
pub fn is_dav_url(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    ["dav://", "davs://", "webdav://", "webdavs://"].iter().any(|p| l.starts_with(p))
}

/// Parsed connection: base URL of the server root (no credentials), auth, start path.
#[derive(Clone, Debug)]
pub struct DavTarget {
    /// `scheme://host[:port]` without a trailing slash.
    pub origin: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Absolute path on the server (percent-decoded).
    pub path: String,
}

pub fn parse_url(input: &str) -> Result<DavTarget> {
    let s = input.trim();
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("davs://").or_else(|| lower.strip_prefix("webdavs://")) {
        ("https", &s[s.len() - r.len()..])
    } else if let Some(r) = lower.strip_prefix("dav://").or_else(|| lower.strip_prefix("webdav://")) {
        ("http", &s[s.len() - r.len()..])
    } else if let Some(r) = lower.strip_prefix("https://") {
        ("https", &s[s.len() - r.len()..])
    } else if let Some(r) = lower.strip_prefix("http://") {
        ("http", &s[s.len() - r.len()..])
    } else {
        bail!("not a WebDAV URL: {input}");
    };
    let u = url::Url::parse(&format!("{scheme}://{rest}")).with_context(|| format!("bad URL: {input}"))?;
    let host = u.host_str().ok_or_else(|| anyhow!("missing host in {input}"))?;
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    let origin = match u.port() {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    };
    let user = (!u.username().is_empty()).then(|| decode(u.username()));
    let password = u.password().map(decode);
    let path = decode(u.path());
    Ok(DavTarget { origin, user, password, path: if path.is_empty() { "/".into() } else { path } })
}

/// Percent-decode (lossy UTF-8).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(hexval), b.get(i + 2).and_then(hexval))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: &u8) -> Option<u8> {
    (*c as char).to_digit(16).map(|d| d as u8)
}

/// Percent-encode a path for a request URL (keep `/`).
fn encode_path(p: &str) -> String {
    let mut out = String::new();
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One `<response>` of a PROPFIND multistatus.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DavItem {
    /// Decoded absolute path from `<href>`.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<std::time::SystemTime>,
}

/// Parse a PROPFIND multistatus body. Namespace prefixes are ignored (only local
/// names are compared), which handles `d:`, `D:`, `lp1:` and default namespaces.
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavItem>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut items = Vec::new();
    let mut cur: Option<DavItem> = None;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut text = String::new();
    // Only take properties from a propstat with a 2xx status.
    let mut prop_ok = true;
    let mut pending: Option<DavItem> = None;
    loop {
        match reader.read_event().map_err(|e| anyhow!("bad PROPFIND XML: {e}"))? {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"response" => cur = Some(DavItem::default()),
                    b"propstat" => {
                        prop_ok = true;
                        pending = cur.clone();
                    }
                    b"collection" if stack.iter().any(|s| s == b"resourcetype") => {
                        if let Some(p) = pending.as_mut() {
                            p.is_dir = true;
                        }
                    }
                    _ => {}
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == b"collection"
                    && stack.iter().any(|s| s == b"resourcetype")
                    && let Some(p) = pending.as_mut()
                {
                    p.is_dir = true;
                }
            }
            Event::Text(t) => {
                let raw = t.decode().map_err(|e| anyhow!("bad PROPFIND XML: {e}"))?;
                text.push_str(&quick_xml::escape::unescape(&raw).map_err(|e| anyhow!("bad PROPFIND XML: {e}"))?);
            }
            Event::GeneralRef(r) => {
                // Entity references (&amp; ...) arrive separately in recent quick-xml.
                let name = r.decode().map_err(|e| anyhow!("bad PROPFIND XML: {e}"))?;
                if let Ok(s) = quick_xml::escape::unescape(&format!("&{name};")) {
                    text.push_str(&s);
                }
            }
            Event::End(e) => {
                let name = e.local_name().as_ref().to_vec();
                stack.pop();
                let t = text.trim();
                match name.as_slice() {
                    b"href" if stack.last().is_some_and(|s| s == b"response") => {
                        if let Some(c) = cur.as_mut() {
                            // href may be a full URL or an absolute path.
                            let path = match t.find("://") {
                                Some(i) => t[i + 3..].find('/').map(|j| &t[i + 3 + j..]).unwrap_or("/"),
                                None => t,
                            };
                            c.path = decode(path);
                        }
                    }
                    b"status" if stack.last().is_some_and(|s| s == b"propstat") => {
                        prop_ok = t.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2'));
                    }
                    b"getcontentlength" => {
                        if let Some(p) = pending.as_mut() {
                            p.size = t.parse().unwrap_or(0);
                        }
                    }
                    b"getlastmodified" => {
                        if let Some(p) = pending.as_mut() {
                            p.mtime = http::parse_http_date(t);
                        }
                    }
                    b"propstat" => {
                        if prop_ok && let (Some(p), Some(c)) = (pending.take(), cur.as_mut()) {
                            c.is_dir |= p.is_dir;
                            if p.size > 0 {
                                c.size = p.size;
                            }
                            if p.mtime.is_some() {
                                c.mtime = p.mtime;
                            }
                        }
                    }
                    b"response" => {
                        if let Some(c) = cur.take()
                            && !c.path.is_empty()
                        {
                            items.push(c);
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(items)
}

pub struct DavFs {
    agent: ureq::Agent,
    origin: String,
    auth: Option<String>,
    user: Option<String>,
}

fn trim_slash(p: &str) -> &str {
    if p.len() > 1 { p.trim_end_matches('/') } else { p }
}

impl DavFs {
    pub fn new(t: &DavTarget) -> DavFs {
        let auth = t.user.as_ref().map(|u| {
            let pw = t.password.clone().unwrap_or_default();
            format!("Basic {}", crate::commands::base64(format!("{u}:{pw}").as_bytes()))
        });
        DavFs { agent: http::agent(), origin: t.origin.clone(), auth, user: t.user.clone() }
    }

    fn url(&self, p: &Path, dir: bool) -> String {
        let mut s = encode_path(&normalize(p).to_string_lossy());
        if dir && !s.ends_with('/') {
            s.push('/');
        }
        format!("{}{s}", self.origin)
    }

    fn request(&self, method: &str, url: &str) -> ureq::http::request::Builder {
        let mut b = ureq::http::Request::builder().method(method).uri(url);
        if let Some(a) = &self.auth {
            b = b.header("Authorization", a);
        }
        b
    }

    fn propfind(&self, p: &Path, depth: u8) -> Result<Option<Vec<DavItem>>> {
        let req = self
            .request("PROPFIND", &self.url(p, depth > 0))
            .header("Depth", depth.to_string())
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(PROPFIND_BODY)?;
        let resp = self.agent.run(req).map_err(|e| anyhow!("PROPFIND {}: {e}", p.display()))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let resp = http::check(&format!("cannot read {}", p.display()), resp)?;
        let body = resp.into_body().with_config().limit(256 << 20).read_to_string()?;
        Ok(Some(parse_multistatus(&body)?))
    }

    fn simple(&self, method: &str, p: &Path, dir: bool, what: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let req = self.request(method, &self.url(p, dir)).body(())?;
        let resp = self.agent.run(req).map_err(|e| anyhow!("{what} {}: {e}", p.display()))?;
        http::check(&format!("{what} {}", p.display()), resp)
    }
}

fn to_entry(name: &str, it: &DavItem) -> Entry {
    let mut e = Entry::new(name, if it.is_dir { EntryKind::Dir } else { EntryKind::File });
    e.mode = if it.is_dir { 0o040755 } else { 0o100644 };
    e.size = if it.is_dir { 0 } else { it.size };
    e.mtime = it.mtime;
    e
}

impl Vfs for DavFs {
    fn scheme(&self) -> &'static str {
        "webdav"
    }

    fn display(&self, path: &Path) -> String {
        let (scheme, rest) = match self.origin.split_once("://") {
            Some(("https", r)) => ("davs", r),
            Some((_, r)) => ("dav", r),
            None => ("dav", self.origin.as_str()),
        };
        let user = self.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
        format!("{scheme}://{user}{rest}{}", normalize(path).display())
    }

    fn writable(&self) -> bool {
        true
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let items = self.propfind(dir, 1)?.ok_or_else(|| anyhow!("{}: no such directory", dir.display()))?;
        let me = normalize(dir).to_string_lossy().into_owned();
        let mut out = Vec::new();
        for it in &items {
            let p = trim_slash(&it.path);
            if p == trim_slash(&me) {
                if !it.is_dir {
                    bail!("{}: not a directory", dir.display());
                }
                continue;
            }
            let name = p.rsplit('/').next().unwrap_or(p);
            if !name.is_empty() {
                out.push(to_entry(name, it));
            }
        }
        Ok(out)
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let items = self.propfind(path, 0)?.ok_or_else(|| anyhow!("{}: no such file or directory", path.display()))?;
        let it = items.first().ok_or_else(|| anyhow!("{}: empty PROPFIND response", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        Ok(to_entry(&name, it))
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let resp = self.simple("GET", path, false, "open")?;
        Ok(Box::new(resp.into_body().into_reader()))
    }

    fn create_write(&mut self, path: &Path, _mode: u32) -> Result<Box<dyn std::io::Write + Send>> {
        let (agent, url, auth, disp) =
            (self.agent.clone(), self.url(path, false), self.auth.clone(), path.display().to_string());
        Ok(Box::new(UploadOnClose::new(Box::new(move |f, len| {
            let mut b =
                ureq::http::Request::builder().method("PUT").uri(&url).header("Content-Length", len.to_string());
            if let Some(a) = &auth {
                b = b.header("Authorization", a);
            }
            let req = b.body(ureq::SendBody::from_reader(f))?;
            let resp = agent.run(req).map_err(|e| anyhow!("upload {disp}: {e}"))?;
            http::check(&format!("upload {disp}"), resp).map(|_| ())
        }))?))
    }

    fn mkdir(&mut self, path: &Path, _mode: u32) -> Result<()> {
        self.simple("MKCOL", path, true, "mkdir").map(|_| ())
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        self.simple("DELETE", path, false, "delete").map(|_| ())
    }

    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        // DELETE on a collection is recursive; keep rmdir semantics.
        if !self.list(path)?.is_empty() {
            bail!("{}: directory not empty", path.display());
        }
        self.simple("DELETE", path, true, "rmdir").map(|_| ())
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        let is_dir = self.stat(from).map(|e| e.is_dir()).unwrap_or(false);
        let req = self
            .request("MOVE", &self.url(from, is_dir))
            .header("Destination", self.url(to, is_dir))
            .header("Overwrite", "T")
            .body(())?;
        let resp = self.agent.run(req).map_err(|e| anyhow!("rename {}: {e}", from.display()))?;
        http::check(&format!("rename {}", from.display()), resp).map(|_| ())
    }
}

/// Connect (PROPFIND the start path to verify access and credentials).
pub fn connect(t: &DavTarget) -> Result<Location> {
    let mut fs = DavFs::new(t);
    let start = normalize(&PathBuf::from(&t.path));
    let e = fs.stat(&start).with_context(|| format!("cannot access {}", fs.display(&start)))?;
    let path = if e.is_dir() { start } else { start.parent().map(Path::to_path_buf).unwrap_or_else(|| "/".into()) };
    Ok(Location { vfs: VfsHandle::new(Box::new(fs)), path })
}

pub fn connect_url(url: &str) -> Result<Location> {
    connect(&parse_url(url)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let t = parse_url("davs://bob:s%40cret@cloud.example.com/remote.php/dav/files/bob/").unwrap();
        assert_eq!(t.origin, "https://cloud.example.com");
        assert_eq!((t.user.as_deref(), t.password.as_deref()), (Some("bob"), Some("s@cret")));
        assert_eq!(t.path, "/remote.php/dav/files/bob/");
        let t = parse_url("dav://127.0.0.1:8080").unwrap();
        assert_eq!((t.origin.as_str(), t.path.as_str()), ("http://127.0.0.1:8080", "/"));
        assert!(parse_url("ftp://x").is_err());
        assert!(is_dav_url("WebDAVs://h/") && !is_dav_url("https://h/"));
        assert_eq!(encode_path("/a b/ü"), "/a%20b/%C3%BC");
    }

    #[test]
    fn parses_multistatus_variants() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:lp1="DAV:">
 <D:response><D:href>/dav/dir/</D:href><D:propstat><D:prop>
  <lp1:resourcetype><D:collection/></lp1:resourcetype><lp1:getlastmodified>Tue, 15 Nov 1994 08:12:31 GMT</lp1:getlastmodified>
 </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>http://h/dav/dir/a%20b.txt</D:href><D:propstat><D:prop>
  <D:resourcetype/><D:getcontentlength>12</D:getcontentlength>
 </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 <D:propstat><D:prop><D:getcontentlength>999</D:getcontentlength></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
 </D:response>
 <response xmlns="DAV:"><href>/dav/dir/R&amp;D/</href><propstat><prop><resourcetype><collection/></resourcetype></prop>
  <status>HTTP/1.1 200 OK</status></propstat></response>
</D:multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].is_dir && items[0].mtime.is_some());
        assert_eq!((items[1].path.as_str(), items[1].size, items[1].is_dir), ("/dav/dir/a b.txt", 12, false));
        assert_eq!((items[2].path.as_str(), items[2].is_dir), ("/dav/dir/R&D/", true));
    }
}

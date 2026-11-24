//! Shared HTTP plumbing for the S3 and WebDAV backends (blocking ureq client).

use anyhow::{Result, anyhow};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use ureq::Agent;
use ureq::http::Response;

/// Agent that returns 4xx/5xx as responses (we inspect them) and allows WebDAV verbs.
pub fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent("pairdeck")
        .build()
        .new_agent()
}

/// Turn a non-success response into an error that includes the server's message.
pub fn check(what: &str, resp: Response<ureq::Body>) -> Result<Response<ureq::Body>> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let mut body = String::new();
    let _ = resp.into_body().into_reader().take(4096).read_to_string(&mut body);
    Err(anyhow!("{what}: HTTP {}{}", status.as_u16(), server_message(&body)))
}

/// Extract `<Message>` (S3) or a short text from an error body.
fn server_message(body: &str) -> String {
    let b = body.trim();
    if b.is_empty() {
        return String::new();
    }
    for tag in ["Message", "message", "s:message"] {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        if let (Some(i), Some(j)) = (b.find(&open), b.find(&close))
            && i + open.len() <= j
        {
            return format!(" ({})", &b[i + open.len()..j]);
        }
    }
    if b.starts_with('<') { String::new() } else { format!(" ({})", b.chars().take(200).collect::<String>()) }
}

pub fn temp_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pairdeck-{tag}-{}-{n}", std::process::id()))
}

/// Upload callback for [`UploadOnClose`]: receives the spooled file and its size.
pub type Uploader = Box<dyn FnMut(&mut File, u64) -> Result<()> + Send>;

/// Writer that spools to an unlinked temp file and uploads on `flush()` (the copy
/// job flushes before closing, so errors are reported) or, as a fallback, on drop.
pub struct UploadOnClose {
    file: File,
    dirty: bool,
    upload: Uploader,
}

impl UploadOnClose {
    pub fn new(upload: Uploader) -> Result<Self> {
        let p = temp_path("upload");
        let file = File::options().read(true).write(true).create_new(true).open(&p)?;
        let _ = std::fs::remove_file(&p);
        // Even an empty file must be created on the server.
        Ok(UploadOnClose { file, dirty: true, upload })
    }

    fn send(&mut self) -> Result<()> {
        let len = self.file.seek(std::io::SeekFrom::End(0))?;
        self.file.rewind()?;
        let res = (self.upload)(&mut self.file, len);
        self.file.seek(std::io::SeekFrom::End(0))?;
        res
    }
}

impl Write for UploadOnClose {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.dirty = true;
        self.file.write(b)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.dirty {
            self.send().map_err(|e| std::io::Error::other(format!("{e:#}")))?;
            self.dirty = false;
        }
        Ok(())
    }
}

impl Drop for UploadOnClose {
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.send();
        }
    }
}

/// HTTP dates (RFC 1123, e.g. `Tue, 15 Nov 1994 08:12:31 GMT`).
pub fn parse_http_date(s: &str) -> Option<std::time::SystemTime> {
    jiff::fmt::rfc2822::parse(s.trim()).ok().and_then(|z| z.timestamp().try_into().ok())
}

/// ISO 8601 dates as used by S3 (`2009-10-12T17:50:30.000Z`).
pub fn parse_iso_date(s: &str) -> Option<std::time::SystemTime> {
    s.trim().parse::<jiff::Timestamp>().ok().and_then(|t| t.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_messages() {
        let secs = |t: Option<std::time::SystemTime>| {
            t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs())
        };
        assert_eq!(secs(parse_http_date("Tue, 15 Nov 1994 08:12:31 GMT")), Some(784_887_151));
        assert_eq!(secs(parse_iso_date("2009-10-12T17:50:30.000Z")), Some(1_255_369_830));
        assert_eq!(secs(parse_iso_date(" 2009-10-12T19:50:30+02:00 ")), Some(1_255_369_830));
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_iso_date("2009-13-12T17:50:30Z"), None);
        assert_eq!(server_message("<Error><Message>Access Denied</Message></Error>"), " (Access Denied)");
        assert_eq!(server_message("plain"), " (plain)");
        assert_eq!(server_message("<x/>"), "");
    }
}

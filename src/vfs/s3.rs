//! Amazon S3 (and compatible: MinIO, R2, rclone, Ceph) backend.
//!
//! `s3://bucket/prefix`. Requests are signed with SigV4 by rusty-s3 (presigned
//! URLs) and sent with the blocking ureq client. Directories are key prefixes;
//! `mkdir` creates a zero-byte `dir/` marker object.
//!
//! Credentials, in order: values given explicitly (Connect dialog), environment
//! (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), the
//! profile `AWS_PROFILE` (or `default`) in `~/.aws/credentials`. Region from
//! `AWS_REGION` / `AWS_DEFAULT_REGION` or the profile in `~/.aws/config`,
//! endpoint from `AWS_ENDPOINT_URL_S3` / `AWS_ENDPOINT_URL` or `endpoint_url`.
//! Without credentials requests are unsigned (public buckets).

use super::http::{self, UploadOnClose};
use super::{Entry, EntryKind, Location, Vfs, VfsHandle, normalize};
use anyhow::{Context, Result, anyhow, bail};
use rusty_s3::actions::{ListObjectsV2, S3Action};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SIGN_TTL: Duration = Duration::from_secs(3600);
/// Files larger than this are uploaded with multipart upload.
const MULTIPART_THRESHOLD: u64 = 64 << 20;
const PART_SIZE: usize = 16 << 20;

/// Connection settings; empty fields fall back to the environment / ~/.aws.
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub bucket: String,
    pub path: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    /// Never sign requests (public buckets).
    pub anonymous: bool,
}

/// Parse `s3://bucket/some/prefix`.
pub fn parse_url(url: &str) -> Result<S3Config> {
    let rest = url.trim().strip_prefix("s3://").ok_or_else(|| anyhow!("not an s3:// URL: {url}"))?;
    let (bucket, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if bucket.is_empty() {
        bail!("missing bucket name in {url}");
    }
    Ok(S3Config { bucket: bucket.to_string(), path: path.to_string(), ..S3Config::default() })
}

// ---------------------------------------------------------------- ~/.aws files

/// Minimal INI reader: section name -> key -> value.
fn read_ini(path: &Path) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone()).or_default().insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    out
}

fn aws_file(env: &str, name: &str) -> Option<PathBuf> {
    std::env::var_os(env).map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".aws").join(name)))
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

struct Resolved {
    creds: Option<Credentials>,
    region: String,
    endpoint: Option<String>,
}

fn resolve(cfg: &S3Config) -> Resolved {
    let profile = cfg.profile.clone().or_else(|| env_nonempty("AWS_PROFILE")).unwrap_or_else(|| "default".into());
    let creds_file = aws_file("AWS_SHARED_CREDENTIALS_FILE", "credentials").map(|p| read_ini(&p)).unwrap_or_default();
    let config_file = aws_file("AWS_CONFIG_FILE", "config").map(|p| read_ini(&p)).unwrap_or_default();
    let prof_creds = creds_file.get(&profile);
    let prof_cfg = config_file.get(&if profile == "default" { profile.clone() } else { format!("profile {profile}") });
    let from_prof = |k: &str| prof_creds.and_then(|m| m.get(k)).or_else(|| prof_cfg.and_then(|m| m.get(k))).cloned();

    let creds = if cfg.anonymous {
        None
    } else if let (Some(k), Some(s)) = (cfg.access_key.clone(), cfg.secret_key.clone()) {
        Some(match cfg.session_token.clone() {
            Some(t) => Credentials::new_with_token(k, s, t),
            None => Credentials::new(k, s),
        })
    } else if let (Some(k), Some(s)) = (env_nonempty("AWS_ACCESS_KEY_ID"), env_nonempty("AWS_SECRET_ACCESS_KEY")) {
        Some(match env_nonempty("AWS_SESSION_TOKEN") {
            Some(t) => Credentials::new_with_token(k, s, t),
            None => Credentials::new(k, s),
        })
    } else if let (Some(k), Some(s)) = (from_prof("aws_access_key_id"), from_prof("aws_secret_access_key")) {
        Some(match from_prof("aws_session_token") {
            Some(t) => Credentials::new_with_token(k, s, t),
            None => Credentials::new(k, s),
        })
    } else {
        None
    };
    let region = cfg
        .region
        .clone()
        .or_else(|| env_nonempty("AWS_REGION"))
        .or_else(|| env_nonempty("AWS_DEFAULT_REGION"))
        .or_else(|| from_prof("region"))
        .unwrap_or_else(|| "us-east-1".into());
    let endpoint = cfg
        .endpoint
        .clone()
        .or_else(|| env_nonempty("AWS_ENDPOINT_URL_S3"))
        .or_else(|| env_nonempty("AWS_ENDPOINT_URL"))
        .or_else(|| from_prof("endpoint_url"));
    Resolved { creds, region, endpoint }
}

// ---------------------------------------------------------------- file system

pub struct S3Fs {
    bucket: Bucket,
    creds: Option<Credentials>,
    agent: ureq::Agent,
}

fn key_of(p: &Path) -> String {
    normalize(p).to_string_lossy().trim_start_matches('/').to_string()
}

fn dir_prefix(p: &Path) -> String {
    let k = key_of(p);
    if k.is_empty() { k } else { format!("{k}/") }
}

fn dir_entry(name: &str) -> Entry {
    let mut e = Entry::new(name, EntryKind::Dir);
    e.mode = 0o040755;
    e
}

impl S3Fs {
    pub fn new(cfg: &S3Config) -> Result<S3Fs> {
        let r = resolve(cfg);
        let (endpoint, style) = match &r.endpoint {
            // Custom endpoints (MinIO, rclone, Ceph...) almost always want path style.
            Some(e) => (e.clone(), UrlStyle::Path),
            None if cfg.bucket.contains('.') => (format!("https://s3.{}.amazonaws.com", r.region), UrlStyle::Path),
            None => (format!("https://s3.{}.amazonaws.com", r.region), UrlStyle::VirtualHost),
        };
        let url = url::Url::parse(&endpoint).with_context(|| format!("bad endpoint URL: {endpoint}"))?;
        let bucket = Bucket::new(url, style, cfg.bucket.clone(), r.region)
            .map_err(|e| anyhow!("bad bucket or endpoint: {e}"))?;
        Ok(S3Fs { bucket, creds: r.creds, agent: http::agent() })
    }

    fn get(&self, what: &str, url: url::Url) -> Result<ureq::http::Response<ureq::Body>> {
        let resp = self.agent.get(url.as_str()).call().map_err(|e| anyhow!("{what}: {e}"))?;
        http::check(what, resp)
    }

    /// One ListObjectsV2 sweep (all pages): (objects, common prefixes).
    fn list_raw(
        &self,
        prefix: &str,
        delimiter: bool,
        max: Option<usize>,
    ) -> Result<(Vec<rusty_s3::actions::list_objects_v2::ListObjectsContent>, Vec<String>)> {
        let mut objects = Vec::new();
        let mut prefixes = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut act = self.bucket.list_objects_v2(self.creds.as_ref());
            if !prefix.is_empty() {
                act.with_prefix(prefix);
            }
            if delimiter {
                act.with_delimiter("/");
            }
            if let Some(m) = max {
                act.with_max_keys(m);
            }
            if let Some(t) = &token {
                act.with_continuation_token(t.clone());
            }
            let resp = self.get("list", act.sign(SIGN_TTL))?;
            let body = resp.into_body().with_config().limit(64 << 20).read_to_string()?;
            let parsed =
                ListObjectsV2::parse_response(&body).map_err(|e| anyhow!("bad ListObjectsV2 response: {e}"))?;
            objects.extend(parsed.contents);
            prefixes.extend(parsed.common_prefixes.into_iter().map(|p| p.prefix));
            match parsed.next_continuation_token {
                Some(t) if max.is_none() && !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok((objects, prefixes))
    }

    fn head(&self, key: &str) -> Result<Option<Entry>> {
        let url = self.bucket.head_object(self.creds.as_ref(), key).sign(SIGN_TTL);
        let resp = self.agent.head(url.as_str()).call().map_err(|e| anyhow!("stat {key}: {e}"))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let resp = http::check(&format!("stat {key}"), resp)?;
        let h = resp.headers();
        let name = key.rsplit('/').next().unwrap_or(key).to_string();
        let mut e = Entry::new(name, EntryKind::File);
        e.mode = 0o100644;
        e.size = h.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
        e.mtime = h.get("last-modified").and_then(|v| v.to_str().ok()).and_then(http::parse_http_date);
        Ok(Some(e))
    }

    fn put_bytes(&self, key: &str, data: &[u8]) -> Result<()> {
        let url = self.bucket.put_object(self.creds.as_ref(), key).sign(SIGN_TTL);
        let resp = self.agent.put(url.as_str()).send(data).map_err(|e| anyhow!("upload {key}: {e}"))?;
        http::check(&format!("upload {key}"), resp).map(|_| ())
    }

    fn delete_key(&self, key: &str) -> Result<()> {
        let url = self.bucket.delete_object(self.creds.as_ref(), key).sign(SIGN_TTL);
        let resp = self.agent.delete(url.as_str()).call().map_err(|e| anyhow!("delete {key}: {e}"))?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        http::check(&format!("delete {key}"), resp).map(|_| ())
    }

    fn uploader(&self, key: String) -> http::Uploader {
        let (bucket, creds, agent) = (self.bucket.clone(), self.creds.clone(), self.agent.clone());
        Box::new(move |f, len| upload(&bucket, creds.as_ref(), &agent, &key, f, len))
    }
}

/// Single PUT for small files, multipart upload for large ones.
fn upload(
    bucket: &Bucket,
    creds: Option<&Credentials>,
    agent: &ureq::Agent,
    key: &str,
    f: &mut std::fs::File,
    len: u64,
) -> Result<()> {
    let what = format!("upload {key}");
    if len <= MULTIPART_THRESHOLD {
        let mut data = Vec::with_capacity(len as usize);
        f.read_to_end(&mut data)?;
        let url = bucket.put_object(creds, key).sign(SIGN_TTL);
        let resp = agent.put(url.as_str()).send(&data[..]).map_err(|e| anyhow!("{what}: {e}"))?;
        return http::check(&what, resp).map(|_| ());
    }
    let create = bucket.create_multipart_upload(creds, key).sign(SIGN_TTL);
    let resp = agent.post(create.as_str()).send_empty().map_err(|e| anyhow!("{what}: {e}"))?;
    let body = http::check(&what, resp)?.into_body().read_to_string()?;
    let upload_id =
        parse_upload_id(&body).ok_or_else(|| anyhow!("{what}: no UploadId in CreateMultipartUpload response"))?;
    let result = (|| -> Result<()> {
        let mut etags = Vec::new();
        let mut buf = vec![0u8; PART_SIZE];
        let mut part: u16 = 1;
        loop {
            let mut n = 0;
            while n < PART_SIZE {
                let r = f.read(&mut buf[n..])?;
                if r == 0 {
                    break;
                }
                n += r;
            }
            if n == 0 {
                break;
            }
            let url = bucket.upload_part(creds, key, part, &upload_id).sign(SIGN_TTL);
            let resp = agent.put(url.as_str()).send(&buf[..n]).map_err(|e| anyhow!("{what}: {e}"))?;
            let resp = http::check(&what, resp)?;
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("{what}: part {part} without ETag"))?
                .to_string();
            etags.push(etag);
            part += 1;
        }
        let done = bucket.complete_multipart_upload(creds, key, &upload_id, etags.iter().map(String::as_str));
        let url = done.sign(SIGN_TTL);
        let resp = agent.post(url.as_str()).send(done.body()).map_err(|e| anyhow!("{what}: {e}"))?;
        // S3 can return 200 with an <Error> body for CompleteMultipartUpload.
        let text = http::check(&what, resp)?.into_body().read_to_string()?;
        if text.contains("<Error>") {
            bail!("{what}: {text}");
        }
        Ok(())
    })();
    if result.is_err() {
        let url = bucket.abort_multipart_upload(creds, key, &upload_id).sign(SIGN_TTL);
        let _ = agent.delete(url.as_str()).call();
    }
    result
}

impl Vfs for S3Fs {
    fn scheme(&self) -> &'static str {
        "s3"
    }

    fn display(&self, path: &Path) -> String {
        format!("s3://{}{}", self.bucket.name(), normalize(path).display())
    }

    fn writable(&self) -> bool {
        true
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let prefix = dir_prefix(dir);
        let (objects, prefixes) = self.list_raw(&prefix, true, None)?;
        let mut out = Vec::new();
        for p in prefixes {
            let name = p.strip_prefix(&prefix).unwrap_or(&p).trim_end_matches('/');
            if !name.is_empty() {
                out.push(dir_entry(name));
            }
        }
        for o in objects {
            let name = o.key.strip_prefix(&prefix).unwrap_or(&o.key);
            // The directory marker itself, and keys with a trailing slash (other markers).
            if name.is_empty() || name.ends_with('/') {
                continue;
            }
            let mut e = Entry::new(name, EntryKind::File);
            e.mode = 0o100644;
            e.size = o.size.unwrap_or(0);
            e.mtime = o.last_modified.as_deref().and_then(http::parse_iso_date);
            out.push(e);
        }
        if out.is_empty() && !prefix.is_empty() && self.head(&key_of(dir))?.is_some() {
            bail!("{}: not a directory", dir.display());
        }
        Ok(out)
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let key = key_of(path);
        if key.is_empty() {
            let mut e = dir_entry("/");
            e.name = "/".into();
            return Ok(e);
        }
        let name = key.rsplit('/').next().unwrap_or(&key).to_string();
        if let Some(e) = self.head(&key)? {
            return Ok(e);
        }
        let (objects, prefixes) = self.list_raw(&format!("{key}/"), true, Some(1))?;
        if !objects.is_empty() || !prefixes.is_empty() {
            return Ok(dir_entry(&name));
        }
        bail!("{}: no such file or directory", path.display())
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let key = key_of(path);
        let url = self.bucket.get_object(self.creds.as_ref(), &key).sign(SIGN_TTL);
        let resp = self.get(&format!("open {key}"), url)?;
        Ok(Box::new(resp.into_body().into_reader()))
    }

    fn create_write(&mut self, path: &Path, _mode: u32) -> Result<Box<dyn std::io::Write + Send>> {
        let key = key_of(path);
        if key.is_empty() {
            bail!("cannot write the bucket root");
        }
        Ok(Box::new(UploadOnClose::new(self.uploader(key))?))
    }

    fn mkdir(&mut self, path: &Path, _mode: u32) -> Result<()> {
        let key = key_of(path);
        if key.is_empty() {
            bail!("the bucket root already exists");
        }
        if self.stat(path).is_ok() {
            bail!("{}: already exists", path.display());
        }
        self.put_bytes(&format!("{key}/"), b"")
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        self.delete_key(&key_of(path))
    }

    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        let prefix = dir_prefix(path);
        if prefix.is_empty() {
            bail!("cannot remove the bucket root");
        }
        let (objects, prefixes) = self.list_raw(&prefix, true, Some(2))?;
        if !prefixes.is_empty() || objects.iter().any(|o| o.key != prefix) {
            bail!("{}: directory not empty", path.display());
        }
        self.delete_key(&prefix)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        let (src, dst) = (key_of(from), key_of(to));
        if self.head(&src)?.is_none() {
            bail!("renaming directories on S3 is not supported (copy and delete instead)");
        }
        // Server-side copy, then delete the source.
        let mut act = self.bucket.put_object(self.creds.as_ref(), &dst);
        let source = format!("/{}/{}", self.bucket.name(), urlencode_key(&src));
        act.headers_mut().insert("x-amz-copy-source", source.clone());
        let url = act.sign(SIGN_TTL);
        let resp = self
            .agent
            .put(url.as_str())
            .header("x-amz-copy-source", &source)
            .send_empty()
            .map_err(|e| anyhow!("rename {src}: {e}"))?;
        let text = http::check(&format!("rename {src}"), resp)?.into_body().read_to_string().unwrap_or_default();
        if text.contains("<Error>") {
            bail!("rename {src}: {text}");
        }
        self.delete_key(&src)
    }
}

/// UploadId from a CreateMultipartUpload response. Strict parse first; some servers
/// (rclone's gofakes3) name the root element differently, so fall back to the tag.
pub fn parse_upload_id(xml: &str) -> Option<String> {
    if let Ok(r) = rusty_s3::actions::CreateMultipartUpload::parse_response(xml) {
        return Some(r.upload_id().to_string());
    }
    let start = xml.find("<UploadId>")? + "<UploadId>".len();
    let end = start + xml[start..].find("</UploadId>")?;
    let id = xml[start..end].trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Percent-encode an object key for `x-amz-copy-source` (keep `/`).
fn urlencode_key(k: &str) -> String {
    let mut out = String::new();
    for b in k.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Connect and check that the bucket is reachable.
pub fn connect(cfg: &S3Config) -> Result<Location> {
    let fs = S3Fs::new(cfg)?;
    fs.list_raw("", true, Some(1)).with_context(|| format!("cannot access s3://{}", cfg.bucket))?;
    let path = normalize(Path::new(if cfg.path.is_empty() { "/" } else { &cfg.path }));
    Ok(Location { vfs: VfsHandle::new(Box::new(fs)), path })
}

/// `cd s3://bucket/prefix` (credentials from the environment / ~/.aws).
pub fn connect_url(url: &str) -> Result<Location> {
    connect(&parse_url(url)?)
}

/// Parse a ListObjectsV2 XML body (public for fuzzing).
pub fn parse_list_response(xml: &str) -> Result<(Vec<String>, Vec<String>)> {
    let r = ListObjectsV2::parse_response(xml).map_err(|e| anyhow!("{e}"))?;
    Ok((r.contents.into_iter().map(|c| c.key).collect(), r.common_prefixes.into_iter().map(|p| p.prefix).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_keys_and_ini() {
        let c = parse_url("s3://my-bucket/a/b").unwrap();
        assert_eq!((c.bucket.as_str(), c.path.as_str()), ("my-bucket", "/a/b"));
        assert_eq!(parse_url("s3://b").unwrap().path, "/");
        assert!(parse_url("s3:///x").is_err());
        assert_eq!(key_of(Path::new("/a/../b/c")), "b/c");
        assert_eq!(dir_prefix(Path::new("/")), "");
        assert_eq!(dir_prefix(Path::new("/x/y")), "x/y/");
        assert_eq!(urlencode_key("a b/c+d"), "a%20b/c%2Bd");
        let dir = std::env::temp_dir().join(format!("pairdeck-ini-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c"), "[default]\naws_access_key_id = AK\n[profile dev]\nregion=eu-west-1\n").unwrap();
        let ini = read_ini(&dir.join("c"));
        assert_eq!(ini["default"]["aws_access_key_id"], "AK");
        assert_eq!(ini["profile dev"]["region"], "eu-west-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_credentials_win() {
        let cfg = S3Config {
            bucket: "b".into(),
            endpoint: Some("http://127.0.0.1:9".into()),
            region: Some("r1".into()),
            access_key: Some("K".into()),
            secret_key: Some("S".into()),
            ..S3Config::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.creds.as_ref().map(|c| c.key()), Some("K"));
        assert_eq!(r.region, "r1");
        assert!(resolve(&S3Config { anonymous: true, ..cfg }).creds.is_none());
    }

    #[test]
    fn parses_list_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>d/</Prefix>
<KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys><Delimiter>/</Delimiter><IsTruncated>false</IsTruncated>
<Contents><Key>d/f.txt</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified><Size>5</Size></Contents>
<CommonPrefixes><Prefix>d/sub/</Prefix></CommonPrefixes></ListBucketResult>"#;
        let (keys, prefixes) = parse_list_response(xml).unwrap();
        assert_eq!(keys, ["d/f.txt"]);
        assert_eq!(prefixes, ["d/sub/"]);
    }
}

//! WebDAV client (RFC 4918) over HTTP/HTTPS.
//!
//! A server is addressed by a base URL, e.g.
//! `https://cloud.example.com/remote.php/dav/files/alice/`. Paths used by
//! this client are relative to that base and always start with `/`.
//! Authentication is HTTP Basic, so HTTPS should be used.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use file_dermaga_core::{DateTime, DirEntry, FLAG_DIR};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use url::Url;

/// Characters kept as-is in a path segment (RFC 3986 unreserved).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

#[derive(Clone, Debug)]
pub struct WebdavConfig {
    /// Base URL of the WebDAV root (http:// or https://).
    pub url: Url,
    pub user: String,
    pub password: String,
    pub accept_invalid_certs: bool,
    pub timeout: Duration,
}

impl WebdavConfig {
    /// `url` may use `http(s)://` or `dav(s)://`/`webdav(s)://` schemes.
    pub fn new(url: &str) -> Result<Self> {
        let fixed = if let Some(rest) = url.strip_prefix("davs://").or_else(|| url.strip_prefix("webdavs://")) {
            format!("https://{rest}")
        } else if let Some(rest) = url.strip_prefix("dav://").or_else(|| url.strip_prefix("webdav://")) {
            format!("http://{rest}")
        } else {
            url.to_owned()
        };
        let mut u = Url::parse(&fixed).with_context(|| format!("invalid WebDAV URL {url:?}"))?;
        if !matches!(u.scheme(), "http" | "https") {
            bail!("unsupported WebDAV URL scheme {:?}", u.scheme());
        }
        if !u.path().ends_with('/') {
            let p = format!("{}/", u.path());
            u.set_path(&p);
        }
        u.set_query(None);
        u.set_fragment(None);
        Ok(Self {
            url: u,
            user: String::new(),
            password: String::new(),
            accept_invalid_certs: false,
            timeout: Duration::from_secs(30),
        })
    }
}

#[derive(Clone)]
pub struct WebdavClient {
    http: reqwest::Client,
    base: Url,
    user: String,
    password: String,
    /// `Server` header of the first response, for diagnostics.
    pub server: String,
    /// Digest challenge once the server asked for Digest auth (shared by clones).
    digest: Arc<Mutex<Option<Digest>>>,
    /// A request has passed authentication, so the scheme is settled.
    auth_settled: Arc<std::sync::atomic::AtomicBool>,
}

/// HTTP Digest authentication state (RFC 7616).
#[derive(Clone, Debug)]
struct Digest {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    algorithm: String,
    qop_auth: bool,
    nc: u32,
}

/// Parse `key=value, key="quoted, value"` parameters of an auth challenge.
fn auth_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect::<String>().trim().to_ascii_lowercase();
        if key.is_empty() {
            break;
        }
        let mut val = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => val.extend(chars.next()),
                    '"' => break,
                    c => val.push(c),
                }
            }
        } else {
            while let Some(c) = chars.peek() {
                if *c == ',' {
                    break;
                }
                val.push(*c);
                chars.next();
            }
        }
        out.push((key, val.trim().to_owned()));
    }
    out
}

/// The Digest challenge from a 401 response, if any.
fn digest_challenge(headers: &reqwest::header::HeaderMap) -> Option<Digest> {
    for v in headers.get_all(reqwest::header::WWW_AUTHENTICATE) {
        let v = v.to_str().ok()?;
        let Some(rest) = v.get(..7).filter(|p| p.eq_ignore_ascii_case("digest ")).map(|_| &v[7..]) else { continue };
        let params = auth_params(rest);
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let algorithm = get("algorithm").unwrap_or_else(|| "MD5".into());
        if !matches!(algorithm.to_ascii_uppercase().as_str(), "MD5" | "MD5-SESS" | "SHA-256" | "SHA-256-SESS") {
            continue;
        }
        return Some(Digest {
            realm: get("realm").unwrap_or_default(),
            nonce: get("nonce")?,
            opaque: get("opaque"),
            algorithm,
            qop_auth: get("qop").is_some_and(|q| q.split(',').any(|x| x.trim() == "auth")),
            nc: 0,
        });
    }
    None
}

fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{:016x}", h.finish())
}

impl Digest {
    fn hash(&self, data: &str) -> String {
        if self.algorithm.to_ascii_uppercase().starts_with("SHA-256") {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(data.as_bytes()))
        } else {
            format!("{:x}", md5::compute(data.as_bytes()))
        }
    }

    /// `Authorization` header value for one request.
    fn authorization(&mut self, user: &str, password: &str, method: &str, uri: &str) -> String {
        self.nc += 1;
        let nc = format!("{:08x}", self.nc);
        let cnonce = random_hex();
        let mut ha1 = self.hash(&format!("{user}:{}:{password}", self.realm));
        if self.algorithm.to_ascii_uppercase().ends_with("-SESS") {
            ha1 = self.hash(&format!("{ha1}:{}:{cnonce}", self.nonce));
        }
        let ha2 = self.hash(&format!("{method}:{uri}"));
        let response = if self.qop_auth {
            self.hash(&format!("{ha1}:{}:{nc}:{cnonce}:auth:{ha2}", self.nonce))
        } else {
            self.hash(&format!("{ha1}:{}:{ha2}", self.nonce))
        };
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut h = format!(
            r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", algorithm={}, response="{response}""#,
            q(user),
            q(&self.realm),
            q(&self.nonce),
            q(uri),
            self.algorithm
        );
        if self.qop_auth {
            h.push_str(&format!(r#", qop=auth, nc={nc}, cnonce="{cnonce}""#));
        }
        if let Some(o) = &self.opaque {
            h.push_str(&format!(r#", opaque="{}""#, q(o)));
        }
        h
    }
}

/// Hrefs and statuses of failed members in a 207 Multi-Status body (e.g. a
/// DELETE that removed only part of a collection).
fn multistatus_failures(xml: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let (mut href, mut text, mut in_propstat) = (String::new(), String::new(), false);
    let mut out = Vec::new();
    while let Ok(ev) = reader.read_event() {
        match ev {
            Event::Start(e) => {
                if e.local_name().as_ref() == "propstat" {
                    in_propstat = true;
                }
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => text.push_str(&resolve_entity(&r.into_inner()).unwrap_or_default()),
            Event::End(e) => {
                match e.local_name().as_ref() {
                    "href" if !in_propstat => href = text.trim().to_owned(),
                    "propstat" => in_propstat = false,
                    "status" if !in_propstat => {
                        let code = text.split_whitespace().nth(1).unwrap_or("");
                        if !code.starts_with('2') {
                            out.push((href.clone(), text.trim().to_owned()));
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
    out
}

#[derive(Default, Debug, Clone)]
struct Prop {
    href: String,
    is_dir: bool,
    size: Option<i64>,
    mtime: Option<DateTime>,
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

fn status_error(status: StatusCode, what: &str) -> anyhow::Error {
    let hint = match status.as_u16() {
        401 => "authentication failed (wrong user name or password)",
        403 => "access denied",
        404 => "not found",
        405 => "operation not allowed here (does it already exist?)",
        409 => "conflict (does the parent directory exist?)",
        412 => "precondition failed (target already exists?)",
        423 => "resource is locked",
        507 => "insufficient storage on the server",
        _ => status.canonical_reason().unwrap_or("request failed"),
    };
    anyhow!("{what}: HTTP {} — {hint}", status.as_u16())
}

/// Parse an HTTP date (RFC 7231 IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`).
fn parse_http_date(s: &str) -> Option<DateTime> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    // Accept with or without the weekday.
    let p = if parts.len() >= 6 { &parts[1..] } else { &parts[..] };
    if p.len() < 4 {
        return None;
    }
    let day: i64 = p[0].parse().ok()?;
    let month = match p[1].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = p[2].parse().ok()?;
    let mut hms = p[3].split(':').map(|x| x.parse::<i64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    DateTime::from_utc(year, month, day, Some(h), Some(m), Some(sec))
}

fn resolve_entity(name: &str) -> Option<String> {
    Some(match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => {
            let code = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)?.to_string()
        }
    })
}

/// Parse a 207 Multi-Status PROPFIND response.
fn parse_multistatus(xml: &str) -> Result<Vec<Prop>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut cur: Option<Prop> = None;
    // Properties of the current <propstat>, applied if its status is 200.
    let mut ps = Prop::default();
    let mut ps_ok = false;
    let mut in_propstat = false;
    let mut text = String::new();

    loop {
        match reader.read_event().context("invalid PROPFIND XML")? {
            Event::Start(e) => {
                match e.local_name().as_ref() {
                    "response" => cur = Some(Prop::default()),
                    "propstat" => {
                        in_propstat = true;
                        ps = Prop::default();
                        ps_ok = false;
                    }
                    "collection" => ps.is_dir = true,
                    _ => {}
                }
                text.clear();
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == "collection" {
                    ps.is_dir = true;
                }
            }
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::CData(c) => text.push_str(&c.into_inner()),
            Event::GeneralRef(r) => {
                let name = r.into_inner();
                text.push_str(&resolve_entity(&name).unwrap_or_default());
            }
            Event::End(e) => {
                let t = text.trim();
                match e.local_name().as_ref() {
                    "href" if !in_propstat => {
                        if let Some(c) = cur.as_mut() {
                            c.href = t.to_owned();
                        }
                    }
                    "getcontentlength" => ps.size = t.parse().ok(),
                    "getlastmodified" => ps.mtime = parse_http_date(t),
                    "status" if in_propstat => ps_ok = t.split_whitespace().nth(1) == Some("200"),
                    "propstat" => {
                        in_propstat = false;
                        if ps_ok {
                            if let Some(c) = cur.as_mut() {
                                c.is_dir |= ps.is_dir;
                                c.size = c.size.or(ps.size);
                                c.mtime = c.mtime.or(ps.mtime);
                            }
                        }
                    }
                    "response" => {
                        if let Some(c) = cur.take() {
                            out.push(c);
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
    Ok(out)
}

/// Decoded path of an href (which may be absolute or relative to `base`).
fn href_path(base: &Url, href: &str) -> Option<String> {
    let u = base.join(href).ok()?;
    Some(percent_decode_str(u.path()).decode_utf8_lossy().into_owned())
}

/// Counts bytes read so progress can be reported while reqwest streams the body.
struct Counting<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Counting<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = (buf.filled().len() - before) as u64;
        self.count.fetch_add(n, Ordering::Relaxed);
        r
    }
}

impl WebdavClient {
    /// Create a client without contacting the server.
    pub fn new(cfg: &WebdavConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.timeout)
            .danger_accept_invalid_certs(cfg.accept_invalid_certs)
            .user_agent(concat!("FileDermaga/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: cfg.url.clone(),
            user: cfg.user.clone(),
            password: cfg.password.clone(),
            server: String::new(),
            digest: Arc::new(Mutex::new(None)),
            auth_settled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Connect and verify the base URL is a readable collection.
    pub async fn connect(cfg: &WebdavConfig) -> Result<Self> {
        let mut c = Self::new(cfg)?;
        let resp = c
            .send(Method::from_bytes(b"PROPFIND")?, cfg.url.clone(), |r| {
                r.header("Depth", "0")
                    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(PROPFIND_BODY)
            })
            .await
            .with_context(|| format!("could not reach {}", cfg.url))?;
        c.server = resp
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let status = resp.status();
        if status != StatusCode::MULTI_STATUS {
            if status.is_success() {
                bail!("{} did not answer like a WebDAV server (HTTP {})", cfg.url, status.as_u16());
            }
            return Err(status_error(status, &format!("connecting to {}", cfg.url)));
        }
        Ok(c)
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// Whether the server switched this client to Digest authentication.
    pub fn uses_digest(&self) -> bool {
        self.digest.lock().unwrap().is_some()
    }

    /// A request with credentials: Digest once the server asked for it,
    /// otherwise Basic.
    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut uri = url.path().to_owned();
        if let Some(q) = url.query() {
            uri.push('?');
            uri.push_str(q);
        }
        let auth = self
            .digest
            .lock()
            .unwrap()
            .as_mut()
            .map(|d| d.authorization(&self.user, &self.password, method.as_str(), &uri));
        let r = self.http.request(method, url);
        match auth {
            Some(a) => r.header(reqwest::header::AUTHORIZATION, a),
            None if self.user.is_empty() => r,
            None => r.basic_auth(&self.user, Some(&self.password)),
        }
    }

    /// Send a request; on a 401 Digest challenge (first time or a stale
    /// nonce) switch to Digest and send it once more.
    async fn send(
        &self,
        method: Method,
        url: Url,
        build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        // Until the auth scheme is known the server may answer 401 without
        // reading our request body; never reuse such a connection, or the
        // leftover body corrupts the retried request.
        let probing = !self.user.is_empty() && !self.uses_digest();
        let mut first = build(self.request(method.clone(), url.clone()));
        if probing {
            first = first.header(reqwest::header::CONNECTION, "close");
        }
        let resp = first.send().await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            self.auth_settled.store(true, Ordering::Relaxed);
        }
        if resp.status() != StatusCode::UNAUTHORIZED || self.user.is_empty() {
            return Ok(resp);
        }
        let Some(challenge) = digest_challenge(resp.headers()) else {
            return Ok(resp);
        };
        *self.digest.lock().unwrap() = Some(challenge);
        let resp = build(self.request(method, url)).send().await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            self.auth_settled.store(true, Ordering::Relaxed);
        }
        Ok(resp)
    }

    /// URL of a path below the base; `dir` adds the trailing slash that
    /// collections use.
    pub fn url_for(&self, path: &str, dir: bool) -> Url {
        let mut p = self.base.path().trim_end_matches('/').to_owned();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            p.push('/');
            p.push_str(&utf8_percent_encode(seg, SEGMENT).to_string());
        }
        if dir || p.is_empty() {
            p.push('/');
        }
        let mut u = self.base.clone();
        u.set_path(&p);
        u
    }

    async fn propfind(&self, path: &str, depth: &str) -> Result<Vec<Prop>> {
        let url = self.url_for(path, true);
        let resp = self
            .send(Method::from_bytes(b"PROPFIND")?, url, |r| {
                r.header("Depth", depth)
                    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(PROPFIND_BODY)
            })
            .await?;
        if resp.status() != StatusCode::MULTI_STATUS {
            return Err(status_error(resp.status(), &format!("listing {path}")));
        }
        parse_multistatus(&resp.text().await?)
    }

    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let props = self.propfind(path, "1").await?;
        let own = percent_decode_str(self.url_for(path, true).path()).decode_utf8_lossy().trim_end_matches('/').to_owned();
        let mut out = Vec::new();
        for p in props {
            let Some(full) = href_path(&self.base, &p.href) else { continue };
            let trimmed = full.trim_end_matches('/');
            if trimmed == own || trimmed.is_empty() {
                continue; // the listed collection itself
            }
            let name = trimmed.rsplit('/').next().unwrap_or("").to_owned();
            if name.is_empty() {
                continue;
            }
            out.push(DirEntry {
                name,
                size: if p.is_dir { -1 } else { p.size.unwrap_or(-1) },
                time: p.mtime.unwrap_or_default(),
                flags: if p.is_dir { FLAG_DIR } else { 0 },
                ..Default::default()
            });
        }
        Ok(out)
    }

    pub async fn is_dir(&self, path: &str) -> Result<bool> {
        match self.propfind(path, "0").await {
            Ok(props) => Ok(props.first().is_some_and(|p| p.is_dir)),
            Err(_) => Ok(false),
        }
    }

    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        path: &str,
        out: &mut W,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let mut resp = self.send(Method::GET, self.url_for(path, false), |r| r).await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), &format!("downloading {path}")));
        }
        let mut total = 0u64;
        while let Some(chunk) = resp.chunk().await.with_context(|| format!("downloading {path}"))? {
            out.write_all(&chunk).await?;
            total += chunk.len() as u64;
            progress(total);
        }
        out.flush().await?;
        Ok(total)
    }

    /// Upload `size` bytes from `input` (a file) to `path`.
    pub async fn upload<R: AsyncRead + Unpin + Send + 'static>(
        &self,
        path: &str,
        input: R,
        size: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        // A streamed body cannot be sent twice, so settle authentication
        // (e.g. switch to Digest) with a cheap request first.
        if !self.user.is_empty() && !self.auth_settled.load(Ordering::Relaxed) {
            let parent = match path.trim_end_matches('/').rfind('/') {
                Some(i) => &path[..i],
                None => "",
            };
            let _ = self
                .send(Method::from_bytes(b"PROPFIND")?, self.url_for(parent, true), |r| {
                    r.header("Depth", "0")
                        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                        .body(PROPFIND_BODY)
                })
                .await;
        }
        let count = Arc::new(AtomicU64::new(0));
        let stream = tokio_util::io::ReaderStream::with_capacity(Counting { inner: input, count: count.clone() }, 256 * 1024);
        let send = self
            .request(Method::PUT, self.url_for(path, false))
            .header(CONTENT_LENGTH, HeaderValue::from(size))
            .body(reqwest::Body::wrap_stream(stream))
            .send();
        tokio::pin!(send);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let resp = loop {
            tokio::select! {
                r = &mut send => break r?,
                _ = tick.tick() => progress(count.load(Ordering::Relaxed)),
            }
        };
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), &format!("uploading {path}")));
        }
        let sent = count.load(Ordering::Relaxed);
        progress(sent);
        Ok(sent)
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let resp = self.send(Method::from_bytes(b"MKCOL")?, self.url_for(path, true), |r| r).await?;
        if resp.status() == StatusCode::CREATED || resp.status().is_success() {
            Ok(())
        } else {
            Err(status_error(resp.status(), &format!("creating directory {path}")))
        }
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let resp = self.send(Method::DELETE, self.url_for(path, false), |r| r).await?;
        Self::check_delete(resp, &format!("deleting {path}")).await
    }

    /// WebDAV deletes collections with all their contents; without
    /// `recursive` a non-empty directory is refused first.
    pub async fn rmdir(&self, path: &str, recursive: bool) -> Result<()> {
        if !recursive && !self.list(path).await?.is_empty() {
            bail!("{path} is not empty");
        }
        let resp = self.send(Method::DELETE, self.url_for(path, true), |r| r).await?;
        Self::check_delete(resp, &format!("removing directory {path}")).await
    }

    /// A DELETE may answer 207 with per-member failures (RFC 4918 9.6.1).
    async fn check_delete(resp: reqwest::Response, what: &str) -> Result<()> {
        let status = resp.status();
        if status == StatusCode::MULTI_STATUS {
            let failures = multistatus_failures(&resp.text().await?);
            if let Some((href, st)) = failures.first() {
                bail!("{what}: {} item(s) could not be deleted, e.g. {href} ({st})", failures.len());
            }
            return Ok(());
        }
        if status.is_success() {
            Ok(())
        } else {
            Err(status_error(status, what))
        }
    }

    /// Server-side move; refuses to overwrite an existing target.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let dir = self.is_dir(from).await?;
        let dest = self.url_for(to, dir);
        let resp = self
            .send(Method::from_bytes(b"MOVE")?, self.url_for(from, dir), |r| {
                r.header("Destination", dest.as_str()).header("Overwrite", "F")
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(status_error(resp.status(), &format!("renaming {from} to {to}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_urls() {
        assert_eq!(WebdavConfig::new("davs://cloud.example.com/dav").unwrap().url.as_str(), "https://cloud.example.com/dav/");
        assert_eq!(WebdavConfig::new("dav://10.0.0.5:8080").unwrap().url.as_str(), "http://10.0.0.5:8080/");
        assert!(WebdavConfig::new("ftp://x").is_err());
    }

    #[test]
    fn digest_rfc7616_example() {
        // RFC 7616 section 3.9.1 (MD5 and SHA-256 variants).
        let hdr = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::WWW_AUTHENTICATE, hdr.parse().unwrap());
        let d = digest_challenge(&h).unwrap();
        assert_eq!(d.realm, "http-auth@example.org");
        assert!(d.qop_auth);
        assert_eq!(d.opaque.as_deref(), Some("FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS"));
        // response = H(H(A1):nonce:nc:cnonce:qop:H(A2)) with the RFC's fixed cnonce
        let ha1 = d.hash("Mufasa:http-auth@example.org:Circle of Life");
        let ha2 = d.hash("GET:/dir/index.html");
        let resp = d.hash(&format!("{ha1}:{}:00000001:f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ:auth:{ha2}", d.nonce));
        assert_eq!(resp, "8ca523f5e9506fed4657c9700eebdbec");
    }

    #[test]
    fn multistatus_delete_failures() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/x/locked.txt</d:href><d:status>HTTP/1.1 423 Locked</d:status></d:response></d:multistatus>"#;
        assert_eq!(multistatus_failures(xml), vec![("/dav/x/locked.txt".to_owned(), "HTTP/1.1 423 Locked".to_owned())]);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(DateTime::ymd_hms(1994, 11, 6, 8, 49, 37)));
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn multistatus() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
 <D:response><D:href>/dav/docs/</D:href>
  <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response><D:href>/dav/docs/Tom%20%26%20Jerry.txt</D:href>
  <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>42</D:getcontentlength>
   <D:getlastmodified>Wed, 26 Nov 2025 01:02:03 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  <D:propstat><D:prop><D:quota/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
 </D:response>
 <D:response><D:href>/dav/docs/a&amp;b/</D:href>
  <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
</D:multistatus>"#;
        let p = parse_multistatus(xml).unwrap();
        assert_eq!(p.len(), 3);
        assert!(p[0].is_dir);
        assert_eq!(p[1].href, "/dav/docs/Tom%20%26%20Jerry.txt");
        assert_eq!(p[1].size, Some(42));
        assert_eq!(p[1].mtime, Some(DateTime::ymd_hms(2025, 11, 26, 1, 2, 3)));
        assert_eq!(p[2].href, "/dav/docs/a&b/");
        assert!(p[2].is_dir);
        let base = Url::parse("https://h/dav/").unwrap();
        assert_eq!(href_path(&base, &p[1].href).unwrap(), "/dav/docs/Tom & Jerry.txt");
    }
}

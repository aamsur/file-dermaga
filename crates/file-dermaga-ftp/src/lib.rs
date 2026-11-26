//! Async FTP / FTPS client (RFC 959, RFC 2228/4217 TLS, RFC 2428 EPSV,
//! RFC 3659 MLSD/SIZE/MDTM).
//!
//! Passive mode only. One `FtpClient` owns one control connection; data
//! connections are opened per transfer.

mod stream;

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use file_dermaga_core::{DateTime, DirectoryListing, ListingParser, ServerType};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{lookup_host, TcpStream};
use tokio_rustls::TlsConnector;

pub use stream::Stream;

#[derive(Debug, thiserror::Error)]
pub enum FtpError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("server replied {code}: {text}")]
    Reply { code: u16, text: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

pub type Result<T> = std::result::Result<T, FtpError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// Never use TLS.
    Plain,
    /// Try `AUTH TLS`, fall back to plaintext if the server refuses.
    TryTls,
    /// Require `AUTH TLS` (explicit FTPS, usually port 21).
    ExplicitTls,
    /// TLS from the first byte (implicit FTPS, usually port 990).
    ImplicitTls,
}

#[derive(Clone, Debug)]
pub struct FtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub security: Security,
    pub accept_invalid_certs: bool,
    pub timeout: Duration,
    /// Print commands and replies to stderr.
    pub verbose: bool,
    /// Receives every status line, command and reply (for a message log).
    pub logger: Option<Logger>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    Status,
    Command,
    Response,
    Error,
}

impl LogKind {
    pub fn label(self) -> &'static str {
        match self {
            LogKind::Status => "Status",
            LogKind::Command => "Command",
            LogKind::Response => "Response",
            LogKind::Error => "Error",
        }
    }
}

pub type LogFn = dyn Fn(LogKind, &str) + Send + Sync;

/// Callback for protocol log lines.
#[derive(Clone)]
pub struct Logger(pub Arc<LogFn>);

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logger")
    }
}

impl FtpConfig {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 21,
            user: String::new(),
            password: String::new(),
            security: Security::TryTls,
            accept_invalid_certs: false,
            timeout: Duration::from_secs(20),
            verbose: false,
            logger: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub code: u16,
    /// All reply lines, joined with '\n'.
    pub text: String,
}

impl Reply {
    pub fn class(&self) -> u16 {
        self.code / 100
    }
    fn into_err(self) -> FtpError {
        FtpError::Reply { code: self.code, text: self.text }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Features {
    pub mlsd: bool,
    pub utf8: bool,
    pub size: bool,
    pub mdtm: bool,
    pub epsv: bool,
    pub rest_stream: bool,
    /// `MFMT`: set a file's modification time.
    pub mfmt: bool,
    pub raw: Vec<String>,
}

pub struct FtpClient {
    ctrl: Option<BufReader<Stream>>,
    // Reply parsing state lives here so `read_reply` is cancel-safe: it can
    // be raced against a data-connection TLS handshake in `select!`.
    line_buf: Vec<u8>,
    reply_lines: Vec<String>,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    protect_data: bool,
    peer_ip: IpAddr,
    features: Features,
    epsv_failed: bool,
    timeout: Duration,
    verbose: bool,
    logger: Option<Logger>,
}

async fn with_timeout<T>(d: Duration, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(d, f).await.map_err(|_| FtpError::Timeout(d))?
}

async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = with_timeout(timeout, async {
        Ok(lookup_host((host, port)).await?.collect())
    })
    .await?;
    let mut last = None;
    for addr in addrs {
        match with_timeout(timeout, async { Ok(TcpStream::connect(addr).await?) }).await {
            Ok(s) => {
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| FtpError::Protocol(format!("no addresses for {host}"))))
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Extract `h1,h2,h3,h4,p1,p2` from a 227 reply.
fn parse_pasv(text: &str) -> Option<(Ipv4Addr, u16)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == ','))
        .find_map(|tok| {
            let nums: Vec<u8> = tok.split(',').map(|s| s.parse().ok()).collect::<Option<_>>()?;
            (nums.len() == 6).then_some(nums)
        })
        .map(|n| (Ipv4Addr::new(n[0], n[1], n[2], n[3]), u16::from(n[4]) << 8 | u16::from(n[5])))
}

/// Extract the port from a 229 reply: `229 Entering Extended Passive Mode (|||6446|)`.
fn parse_epsv(text: &str) -> Option<u16> {
    let open = text.find('(')?;
    let inner = &text[open + 1..text[open..].find(')')? + open];
    let delim = inner.chars().next()?;
    let parts: Vec<&str> = inner.split(delim).collect();
    if parts.len() != 5 {
        return None;
    }
    parts[3].parse().ok()
}

/// Join an FTP path and a name.
pub fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn is_eof(e: &io::Error) -> bool {
    // Many servers close the TLS data connection without close_notify.
    e.kind() == io::ErrorKind::UnexpectedEof
}

impl FtpClient {
    /// Connect and log in.
    pub async fn connect(cfg: &FtpConfig) -> Result<Self> {
        let tcp = connect_tcp(&cfg.host, cfg.port, cfg.timeout).await?;
        let peer_ip = tcp.peer_addr()?.ip();

        let tls = if cfg.security == Security::Plain {
            None
        } else {
            let name = ServerName::try_from(cfg.host.clone())
                .map_err(|e| FtpError::Tls(format!("invalid server name: {e}")))?;
            Some((TlsConnector::from(stream::tls_config(cfg.accept_invalid_certs)), name))
        };

        let stream = if cfg.security == Security::ImplicitTls {
            let (connector, name) = tls.as_ref().expect("tls configured");
            let s = with_timeout(cfg.timeout, async {
                connector.connect(name.clone(), tcp).await.map_err(|e| FtpError::Tls(e.to_string()))
            })
            .await?;
            Stream::Tls(Box::new(s))
        } else {
            Stream::Plain(tcp)
        };

        let mut c = Self {
            ctrl: Some(BufReader::new(stream)),
            line_buf: Vec::new(),
            reply_lines: Vec::new(),
            tls,
            protect_data: false,
            peer_ip,
            features: Features::default(),
            epsv_failed: false,
            timeout: cfg.timeout,
            verbose: cfg.verbose,
            logger: cfg.logger.clone(),
        };
        c.log(LogKind::Status, &format!("Connected to {}", SocketAddr::new(peer_ip, cfg.port)));

        let welcome = c.read_reply().await?;
        if welcome.class() != 2 {
            return Err(welcome.into_err());
        }

        if matches!(cfg.security, Security::TryTls | Security::ExplicitTls) {
            let r = c.command("AUTH TLS").await?;
            if r.code == 234 {
                c.upgrade_control().await?;
            } else if cfg.security == Security::ExplicitTls {
                return Err(FtpError::Tls(format!("server refused AUTH TLS: {} {}", r.code, r.text)));
            } else {
                c.log(LogKind::Status, "Server does not support TLS, continuing in plaintext");
                c.tls = None;
            }
        }

        c.login(&cfg.user, &cfg.password).await?;

        if c.ctrl_is_tls() {
            c.expect("PBSZ 0", 2).await?;
            let r = c.command("PROT P").await?;
            c.protect_data = r.class() == 2;
        }

        c.load_features().await?;
        if c.features.utf8 {
            // Best effort; some servers have UTF-8 always on and reject this.
            let _ = c.command("OPTS UTF8 ON").await?;
        }
        c.expect("TYPE I", 2).await?;
        Ok(c)
    }

    pub fn features(&self) -> &Features {
        &self.features
    }

    pub fn is_secure(&self) -> bool {
        self.ctrl_is_tls()
    }

    fn ctrl_is_tls(&self) -> bool {
        self.ctrl.as_ref().is_some_and(|c| c.get_ref().is_tls())
    }

    fn log(&self, kind: LogKind, msg: &str) {
        if let Some(l) = &self.logger {
            (l.0)(kind, msg);
        }
        if self.verbose {
            eprintln!("{:>9}: {msg}", kind.label());
        }
    }

    async fn upgrade_control(&mut self) -> Result<()> {
        let reader = self.ctrl.take().ok_or_else(|| FtpError::Protocol("not connected".into()))?;
        if !reader.buffer().is_empty() {
            return Err(FtpError::Protocol("unexpected data before TLS handshake".into()));
        }
        let Stream::Plain(tcp) = reader.into_inner() else {
            return Err(FtpError::Protocol("control connection already uses TLS".into()));
        };
        let (connector, name) = self.tls.clone().expect("tls configured");
        let tls = with_timeout(self.timeout, async {
            connector.connect(name, tcp).await.map_err(|e| FtpError::Tls(e.to_string()))
        })
        .await?;
        self.ctrl = Some(BufReader::new(Stream::Tls(Box::new(tls))));
        self.log(LogKind::Status, "TLS connection established");
        Ok(())
    }

    async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        let (user, password) = if user.is_empty() {
            ("anonymous", if password.is_empty() { "anonymous@example.com" } else { password })
        } else {
            (user, password)
        };
        let r = self.command(&format!("USER {user}")).await?;
        let r = match r.code {
            230 => return Ok(()),
            331 => self.command(&format!("PASS {password}")).await?,
            _ => return Err(r.into_err()),
        };
        match r.code {
            230 | 202 => Ok(()),
            332 => Err(FtpError::Protocol("server requires ACCT, which is not supported".into())),
            _ => Err(r.into_err()),
        }
    }

    async fn load_features(&mut self) -> Result<()> {
        let r = self.command("FEAT").await?;
        let mut f = Features { epsv: true, ..Default::default() };
        if r.code == 211 {
            for line in r.text.lines().skip(1) {
                let line = line.trim();
                if line.starts_with("211") {
                    continue;
                }
                let upper = line.to_ascii_uppercase();
                let word = upper.split_whitespace().next().unwrap_or("");
                match word {
                    "MLST" | "MLSD" => f.mlsd = true,
                    "UTF8" => f.utf8 = true,
                    "SIZE" => f.size = true,
                    "MDTM" => f.mdtm = true,
                    "REST" if upper.contains("STREAM") => f.rest_stream = true,
                    "MFMT" => f.mfmt = true,
                    _ => {}
                }
                f.raw.push(line.to_owned());
            }
        }
        self.features = f;
        Ok(())
    }

    async fn send(&mut self, cmd: &str) -> Result<()> {
        let shown = if cmd.starts_with("PASS ") { "PASS ****" } else { cmd };
        self.log(LogKind::Command, shown);
        let ctrl = self.ctrl.as_mut().ok_or_else(|| FtpError::Protocol("not connected".into()))?;
        let w = ctrl.get_mut();
        with_timeout(self.timeout, async {
            w.write_all(cmd.as_bytes()).await?;
            w.write_all(b"\r\n").await?;
            w.flush().await?;
            Ok(())
        })
        .await
    }

    /// Read one complete (possibly multi-line) reply. Cancel-safe.
    async fn read_reply(&mut self) -> Result<Reply> {
        let timeout = self.timeout;
        loop {
            let Self { ctrl, line_buf, .. } = self;
            let ctrl = ctrl.as_mut().ok_or_else(|| FtpError::Protocol("not connected".into()))?;
            let n = with_timeout(timeout, async { Ok(ctrl.read_until(b'\n', line_buf).await?) }).await?;
            if n == 0 && !line_buf.ends_with(b"\n") {
                return Err(FtpError::Protocol("connection closed by server".into()));
            }
            let raw = std::mem::take(&mut self.line_buf);
            let line = String::from_utf8_lossy(&raw).trim_end_matches(['\r', '\n']).to_owned();
            self.log(LogKind::Response, &line);
            self.reply_lines.push(line);

            let first = &self.reply_lines[0];
            let code: u16 = match first.get(..3).and_then(|c| c.parse().ok()) {
                Some(c) => c,
                None => {
                    let bad = std::mem::take(&mut self.reply_lines).join("\n");
                    return Err(FtpError::Protocol(format!("malformed reply: {bad}")));
                }
            };
            let multiline = first.as_bytes().get(3) == Some(&b'-');
            let last = self.reply_lines.last().expect("just pushed");
            let done = !multiline
                || (self.reply_lines.len() > 1
                    && last.get(..3) == first.get(..3)
                    && last.as_bytes().get(3) != Some(&b'-'));
            if done {
                let lines = std::mem::take(&mut self.reply_lines);
                let text = lines
                    .iter()
                    .map(|l| l.get(4..).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = if multiline { lines.join("\n") } else { text };
                return Ok(Reply { code, text });
            }
        }
    }

    /// Send a command and return its final reply (skipping 1xx).
    pub async fn command(&mut self, cmd: &str) -> Result<Reply> {
        self.send(cmd).await?;
        loop {
            let r = self.read_reply().await?;
            if r.class() != 1 {
                return Ok(r);
            }
        }
    }

    async fn expect(&mut self, cmd: &str, class: u16) -> Result<Reply> {
        let r = self.command(cmd).await?;
        if r.class() == class {
            Ok(r)
        } else {
            Err(r.into_err())
        }
    }

    async fn open_passive(&mut self) -> Result<TcpStream> {
        if !self.epsv_failed {
            let r = self.command("EPSV").await?;
            if r.code == 229 {
                if let Some(port) = parse_epsv(&r.text) {
                    return connect_tcp(&self.peer_ip.to_string(), port, self.timeout).await;
                }
            }
            self.epsv_failed = true;
        }
        if self.peer_ip.is_ipv6() {
            return Err(FtpError::Protocol("EPSV failed and PASV does not support IPv6".into()));
        }
        let r = self.expect("PASV", 2).await?;
        let (ip, port) =
            parse_pasv(&r.text).ok_or_else(|| FtpError::Protocol(format!("bad PASV reply: {}", r.text)))?;
        // Servers behind NAT often announce their internal address; use the
        // control connection's peer instead in that case.
        let ip = IpAddr::V4(ip);
        let target = if ip.is_unspecified() || (is_private(ip) && !is_private(self.peer_ip)) {
            self.peer_ip
        } else {
            ip
        };
        connect_tcp(&target.to_string(), port, self.timeout).await
    }

    /// Open a data connection and issue `cmd` on it (RETR, STOR, LIST...).
    async fn data_command(&mut self, cmd: &str) -> Result<Stream> {
        let tcp = self.open_passive().await?;
        self.send(cmd).await?;

        if !self.protect_data {
            let r = self.read_reply().await?;
            if r.class() != 1 {
                return Err(r.into_err());
            }
            return Ok(Stream::Plain(tcp));
        }

        // Some servers start the TLS accept before sending 1xx, others after;
        // run the handshake and the reply read concurrently.
        let (connector, name) = self.tls.clone().expect("tls configured");
        let mut handshake = Box::pin(connector.connect(name, tcp));
        let mut tls = None;
        let timeout = self.timeout;
        let reply = with_timeout(timeout, async {
            loop {
                tokio::select! {
                    res = &mut handshake, if tls.is_none() => {
                        tls = Some(res.map_err(|e| FtpError::Tls(format!("data connection: {e}")))?);
                    }
                    r = self.read_reply() => break r,
                }
            }
        })
        .await?;
        if reply.class() != 1 {
            return Err(reply.into_err());
        }
        let tls = match tls {
            Some(t) => t,
            None => with_timeout(timeout, async {
                handshake.await.map_err(|e| FtpError::Tls(format!("data connection: {e}")))
            })
            .await?,
        };
        Ok(Stream::Tls(Box::new(tls)))
    }

    async fn finish_transfer(&mut self) -> Result<()> {
        let r = self.read_reply().await?;
        if r.class() == 2 {
            Ok(())
        } else {
            Err(r.into_err())
        }
    }

    async fn read_all(data: &mut Stream, out: &mut Vec<u8>) -> Result<()> {
        match data.read_to_end(out).await {
            Ok(_) => Ok(()),
            Err(e) if is_eof(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn pwd(&mut self) -> Result<String> {
        let r = self.expect("PWD", 2).await?;
        let start = r.text.find('"');
        let end = r.text.rfind('"');
        match (start, end) {
            (Some(s), Some(e)) if e > s => Ok(r.text[s + 1..e].replace("\"\"", "\"")),
            _ => Err(FtpError::Protocol(format!("bad PWD reply: {}", r.text))),
        }
    }

    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        self.expect(&format!("CWD {path}"), 2).await.map(drop)
    }

    /// List a directory (MLSD if supported, otherwise LIST).
    pub async fn list(&mut self, path: &str) -> Result<DirectoryListing> {
        if !path.is_empty() {
            self.cwd(path).await?;
        }
        let cmd = if self.features.mlsd { "MLSD" } else { "LIST" };
        let mut data = self.data_command(cmd).await?;
        let mut raw = Vec::new();
        Self::read_all(&mut data, &mut raw).await?;
        drop(data);
        self.finish_transfer().await?;
        let mut parser = ListingParser::new(ServerType::Unix);
        parser.feed(&raw);
        let mut listing = parser.parse(path);
        if listing.failed {
            return Err(FtpError::Protocol(
                parser.error().unwrap_or("failed to parse directory listing").to_owned(),
            ));
        }
        listing.entries.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.cmp(&b.name)));
        Ok(listing)
    }

    /// Download `path` into `out`. `progress` receives the running byte count.
    pub async fn retrieve<W: AsyncWrite + Unpin>(
        &mut self,
        path: &str,
        out: &mut W,
        progress: impl FnMut(u64),
    ) -> Result<u64> {
        self.retrieve_from(path, out, 0, progress).await
    }

    /// Download starting at byte `offset` (`REST` + `RETR`), for resuming.
    /// Returns the number of bytes received in this call.
    pub async fn retrieve_from<W: AsyncWrite + Unpin>(
        &mut self,
        path: &str,
        out: &mut W,
        offset: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        if offset > 0 {
            self.expect(&format!("REST {offset}"), 3).await?;
        }
        let mut data = self.data_command(&format!("RETR {path}")).await?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = match data.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if is_eof(&e) => break,
                Err(e) => return Err(e.into()),
            };
            out.write_all(&buf[..n]).await?;
            total += n as u64;
            progress(total);
        }
        out.flush().await?;
        drop(data);
        self.finish_transfer().await?;
        Ok(total)
    }

    /// Upload from `input` to `path`.
    pub async fn store<R: AsyncRead + Unpin>(
        &mut self,
        path: &str,
        input: &mut R,
        progress: impl FnMut(u64),
    ) -> Result<u64> {
        self.store_mode(path, input, false, progress).await
    }

    /// Upload; with `append` the data is added to the end of the remote
    /// file (`APPE`), which is how an interrupted upload is resumed.
    pub async fn store_mode<R: AsyncRead + Unpin>(
        &mut self,
        path: &str,
        input: &mut R,
        append: bool,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let verb = if append { "APPE" } else { "STOR" };
        let mut data = self.data_command(&format!("{verb} {path}")).await?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            data.write_all(&buf[..n]).await?;
            total += n as u64;
            progress(total);
        }
        // Shutting down sends TLS close_notify and FIN, which is how the
        // server learns the upload is complete.
        data.shutdown().await?;
        // Then wait for the server to close its side. Closing a socket that
        // still has unread incoming bytes (e.g. a TLS 1.3 session ticket)
        // makes the kernel send RST, and the server may then discard upload
        // data it has not read yet.
        let mut sink = [0u8; 4096];
        let _ = tokio::time::timeout(self.timeout, async {
            while let Ok(n) = data.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        })
        .await;
        drop(data);
        self.finish_transfer().await?;
        Ok(total)
    }

    /// Set the modification time (`MFMT`); `Ok(false)` if unsupported.
    pub async fn set_mtime(&mut self, path: &str, time: &DateTime) -> Result<bool> {
        if !self.features.mfmt {
            return Ok(false);
        }
        let Some(t) = time.to_tm() else { return Ok(false) };
        let stamp = format!("{:04}{:02}{:02}{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        let r = self.command(&format!("MFMT {stamp} {path}")).await?;
        Ok(r.class() == 2)
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<()> {
        self.expect(&format!("MKD {path}"), 2).await.map(drop)
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<()> {
        self.expect(&format!("RMD {path}"), 2).await.map(drop)
    }

    pub async fn delete(&mut self, path: &str) -> Result<()> {
        self.expect(&format!("DELE {path}"), 2).await.map(drop)
    }

    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.expect(&format!("RNFR {from}"), 3).await?;
        self.expect(&format!("RNTO {to}"), 2).await.map(drop)
    }

    pub async fn chmod(&mut self, path: &str, mode: &str) -> Result<()> {
        self.expect(&format!("SITE CHMOD {mode} {path}"), 2).await.map(drop)
    }

    pub async fn size(&mut self, path: &str) -> Result<Option<u64>> {
        let r = self.command(&format!("SIZE {path}")).await?;
        Ok((r.code == 213).then(|| r.text.trim().parse().ok()).flatten())
    }

    pub async fn mdtm(&mut self, path: &str) -> Result<Option<DateTime>> {
        let r = self.command(&format!("MDTM {path}")).await?;
        Ok((r.code == 213).then(|| DateTime::parse_compact(r.text.trim())).flatten())
    }

    /// Delete a directory tree.
    pub async fn remove_recursive(&mut self, path: &str) -> Result<()> {
        let mut stack = vec![(path.to_owned(), false)];
        while let Some((dir, visited)) = stack.pop() {
            if visited {
                self.rmdir(&dir).await?;
                continue;
            }
            let listing = self.list(&dir).await?;
            stack.push((dir.clone(), true));
            for e in listing.entries {
                let child = join(&dir, &e.name);
                if e.is_dir() && !e.is_link() {
                    stack.push((child, false));
                } else {
                    self.delete(&child).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn quit(mut self) -> Result<()> {
        let _ = self.command("QUIT").await;
        if let Some(mut c) = self.ctrl.take() {
            let _ = c.get_mut().shutdown().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasv_reply() {
        assert_eq!(
            parse_pasv("227 Entering Passive Mode (192,168,1,2,19,137)."),
            Some((Ipv4Addr::new(192, 168, 1, 2), 19 * 256 + 137))
        );
        assert_eq!(
            parse_pasv("227 =10,0,0,1,4,1"),
            Some((Ipv4Addr::new(10, 0, 0, 1), 1025))
        );
        assert_eq!(parse_pasv("227 nonsense"), None);
    }

    #[test]
    fn epsv_reply() {
        assert_eq!(parse_epsv("Entering Extended Passive Mode (|||6446|)"), Some(6446));
        assert_eq!(parse_epsv("Entering Extended Passive Mode (!!!21000!)"), Some(21000));
        assert_eq!(parse_epsv("garbage"), None);
    }

    #[test]
    fn joins() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/x", "a"), "/x/a");
        assert_eq!(join("", "a"), "a");
    }
}

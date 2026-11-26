//! SFTP client (SSH File Transfer Protocol, version 3) built on russh.
//!
//! Authentication tries, in order: an explicit private key file, the
//! password (also as keyboard-interactive), the ssh-agent, and the default
//! keys in `~/.ssh`. Server host keys are checked against an OpenSSH
//! `known_hosts` file: unknown hosts are rejected with their fingerprint
//! (so the caller can ask the user and retry with `trust_fingerprint`), and a
//! changed key is always rejected.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use file_dermaga_core::{Accuracy, DateTime, DirEntry, FLAG_DIR, FLAG_LINK};
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{known_hosts, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::Disconnect;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::RawSftpSession;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

/// Status messages for a log view.
pub type StatusFn = Arc<dyn Fn(&str) + Send + Sync>;

/// One keyboard-interactive request (RFC 4256): show `prompts` (text,
/// echo) and return the answers, or `None` to cancel.
#[derive(Clone, Debug)]
pub struct KbdRequest {
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<(String, bool)>,
}

pub type Prompter =
    Arc<dyn Fn(KbdRequest) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<String>>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct SftpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Private key file (OpenSSH or PEM format).
    pub key_file: Option<PathBuf>,
    pub key_passphrase: Option<String>,
    /// Defaults to `~/.ssh/known_hosts`.
    pub known_hosts: Option<PathBuf>,
    /// SHA256 fingerprint the user accepted for an unknown host; it is
    /// added to `known_hosts` when it matches.
    pub trust_fingerprint: Option<String>,
    /// Accept and remember any unknown host key (like OpenSSH's
    /// `StrictHostKeyChecking=accept-new`). Changed keys are still refused.
    pub accept_new_host_key: bool,
    /// Save a trusted fingerprint to known_hosts (false = this session only).
    pub remember_host_key: bool,
    pub timeout: Duration,
    pub status: Option<StatusFn>,
    /// Answers keyboard-interactive questions (OTP codes etc.).
    pub prompter: Option<Prompter>,
}

impl fmt::Debug for SftpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("key_file", &self.key_file)
            .finish_non_exhaustive()
    }
}

impl SftpConfig {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: String::new(),
            password: String::new(),
            key_file: None,
            key_passphrase: None,
            known_hosts: None,
            trust_fingerprint: None,
            accept_new_host_key: false,
            remember_host_key: true,
            timeout: Duration::from_secs(20),
            status: None,
            prompter: None,
        }
    }

    fn status(&self, msg: &str) {
        if let Some(s) = &self.status {
            s(msg);
        }
    }

    fn known_hosts_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.known_hosts {
            return Ok(p.clone());
        }
        Ok(home_dir()?.join(".ssh").join("known_hosts"))
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| anyhow!("HOME is not set"))
}

/// Why the server's host key was not accepted.
#[derive(Debug, Clone)]
pub enum HostKeyError {
    /// Not in known_hosts yet; the user may choose to trust it.
    Unknown { fingerprint: String, algorithm: String },
    /// Different from the recorded key: possible man-in-the-middle attack.
    Changed { fingerprint: String, known_hosts: PathBuf, line: usize },
    /// Host certificates are not supported.
    Unsupported,
}

impl fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostKeyError::Unknown { fingerprint, algorithm } => write!(
                f,
                "UNKNOWN_HOST_KEY {fingerprint} ({algorithm}): the server's host key is not in known_hosts"
            ),
            HostKeyError::Changed { fingerprint, known_hosts, line } => write!(
                f,
                "HOST_KEY_CHANGED {fingerprint}: the server's host key differs from the one recorded in {}:{line}. \
                 This could be a man-in-the-middle attack. If the server was reinstalled, remove that line first.",
                known_hosts.display()
            ),
            HostKeyError::Unsupported => f.write_str("the server presented an SSH host certificate, which is not supported"),
        }
    }
}

impl std::error::Error for HostKeyError {}

struct Handler {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    trust: Option<String>,
    accept_new: bool,
    remember: bool,
    problem: Arc<Mutex<Option<HostKeyError>>>,
    status: Option<StatusFn>,
}

impl Handler {
    fn learn(&self, key: &PublicKey) {
        if let Err(e) = known_hosts::learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            if let Some(s) = &self.status {
                s(&format!("Could not save the host key to {}: {e}", self.known_hosts.display()));
            }
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            *self.problem.lock().unwrap() = Some(HostKeyError::Unsupported);
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match known_hosts::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Err(russh::keys::Error::KeyChanged { line }) => {
                *self.problem.lock().unwrap() = Some(HostKeyError::Changed {
                    fingerprint,
                    known_hosts: self.known_hosts.clone(),
                    line,
                });
                Ok(false)
            }
            // Not recorded (or no known_hosts file yet).
            _ => {
                let trusted = self.trust.as_deref() == Some(fingerprint.as_str());
                if trusted && !self.remember {
                    if let Some(s) = &self.status {
                        s(&format!("Trusting host key {fingerprint} for this session only"));
                    }
                    Ok(true)
                } else if self.accept_new || trusted {
                    self.learn(key);
                    if let Some(s) = &self.status {
                        s(&format!("Added host key {fingerprint} to {}", self.known_hosts.display()));
                    }
                    Ok(true)
                } else {
                    *self.problem.lock().unwrap() = Some(HostKeyError::Unknown {
                        fingerprint,
                        algorithm: key.algorithm().to_string(),
                    });
                    Ok(false)
                }
            }
        }
    }
}

pub struct SftpClient {
    handle: Handle<Handler>,
    sftp: SftpSession,
    /// Second SFTP channel used for listings: it exposes each entry's
    /// `longname` (ls -l line), the only source of owner/group names in
    /// SFTP v3. `None` if the server refused a second channel.
    raw: Option<RawSftpSession>,
    home: String,
}

/// Owner/group and permission string from an `ls -l` style longname:
/// `drwxr-xr-x  2 alice staff  4096 Jan  1 00:00 name`.
fn parse_longname(longname: &str) -> Option<(String, String)> {
    let t: Vec<&str> = longname.split_whitespace().collect();
    let perms = *t.first()?;
    if perms.len() < 10 || !matches!(perms.as_bytes()[0], b'-' | b'd' | b'l' | b'b' | b'c' | b'p' | b's') || t.len() < 5 {
        return None;
    }
    Some((perms[..10].to_owned(), format!("{} {}", t[2], t[3])))
}

/// Join a remote directory and a name.
pub fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `drwxr-xr-x`-style rendering of a POSIX mode.
pub fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    let exec = |x: u32, special: u32, set: char, unset: char| match (mode & x != 0, mode & special != 0) {
        (true, true) => set,
        (false, true) => unset,
        (true, false) => 'x',
        (false, false) => '-',
    };
    s.push(bit(0o400, 'r'));
    s.push(bit(0o200, 'w'));
    s.push(exec(0o100, 0o4000, 's', 'S'));
    s.push(bit(0o040, 'r'));
    s.push(bit(0o020, 'w'));
    s.push(exec(0o010, 0o2000, 's', 'S'));
    s.push(bit(0o004, 'r'));
    s.push(bit(0o002, 'w'));
    s.push(exec(0o001, 0o1000, 't', 'T'));
    s
}

fn owner_group(a: &FileAttributes) -> String {
    let owner = a.user.clone().or_else(|| a.uid.map(|u| u.to_string()));
    let group = a.group.clone().or_else(|| a.gid.map(|g| g.to_string()));
    match (owner, group) {
        (Some(o), Some(g)) => format!("{o} {g}"),
        (Some(o), None) => o,
        (None, Some(g)) => g,
        (None, None) => String::new(),
    }
}

/// Standard SSH_FX_* name of an SFTP status code.
fn fx_name(code: StatusCode) -> &'static str {
    match code {
        StatusCode::Ok => "SSH_FX_OK",
        StatusCode::Eof => "SSH_FX_EOF",
        StatusCode::NoSuchFile => "SSH_FX_NO_SUCH_FILE",
        StatusCode::PermissionDenied => "SSH_FX_PERMISSION_DENIED",
        StatusCode::Failure => "SSH_FX_FAILURE",
        StatusCode::BadMessage => "SSH_FX_BAD_MESSAGE",
        StatusCode::NoConnection => "SSH_FX_NO_CONNECTION",
        StatusCode::ConnectionLost => "SSH_FX_CONNECTION_LOST",
        StatusCode::OpUnsupported => "SSH_FX_OP_UNSUPPORTED",
    }
}

/// Turn an SFTP error into "SSH_FX_NAME: server text" (whitespace collapsed).
fn fx(e: SftpError) -> anyhow::Error {
    match e {
        SftpError::Status(st) => {
            let text = st.error_message.split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if text.is_empty() { "(the server gave no reason)".to_owned() } else { text };
            anyhow!("{}: {text}", fx_name(st.status_code))
        }
        other => anyhow!(other),
    }
}

fn is_eof(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::UnexpectedEof
}

/// Load a private key (OpenSSH, PEM/PKCS#8 or PuTTY PPK v2/v3). Errors start
/// with KEY_PASSPHRASE_NEEDED / KEY_LOAD_FAILED so a UI can ask for the
/// passphrase and retry.
fn load_key(path: &Path, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
    match russh::keys::load_secret_key(path, passphrase) {
        Ok(k) => Ok(k),
        Err(russh::keys::Error::KeyIsEncrypted) => Err(anyhow!(
            "KEY_PASSPHRASE_NEEDED {}: the private key is protected by a passphrase",
            path.display()
        )),
        Err(e) if passphrase.is_some() => Err(anyhow!(
            "KEY_LOAD_FAILED {}: {e} (wrong passphrase?)",
            path.display()
        )),
        Err(e) => Err(anyhow!("KEY_LOAD_FAILED {}: {e}", path.display())),
    }
}

async fn auth_with_key(
    handle: &mut Handle<Handler>,
    user: &str,
    key: russh::keys::PrivateKey,
) -> Result<bool> {
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    let res = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await?;
    Ok(res.success())
}

async fn auth_with_password(handle: &mut Handle<Handler>, user: &str, password: &str) -> Result<bool> {
    Ok(handle.authenticate_password(user, password).await?.success())
}

/// Is this a plain "Password:" prompt that the stored password answers?
fn is_password_prompt(text: &str, user: &str) -> bool {
    let t = text.trim().trim_end_matches(':').trim().to_ascii_lowercase();
    t == "password" || t.starts_with("password for") || t == format!("{user}'s password").to_ascii_lowercase()
}

/// Keyboard-interactive (RFC 4256). A single "Password:" prompt is answered
/// with the stored password once; anything else (OTP codes, extra prompts)
/// goes to the prompter. Rejected answers restart the exchange, up to 3
/// times, so a mistyped code can be re-entered. `Err` = cancelled.
async fn auth_keyboard_interactive(
    handle: &mut Handle<Handler>,
    user: &str,
    password: &str,
    prompter: Option<&Prompter>,
) -> Result<bool> {
    let mut password_sent = false;
    for _attempt in 0..3 {
        let mut resp = handle.authenticate_keyboard_interactive_start(user, None::<String>).await?;
        let mut asked_user = false;
        for _ in 0..10 {
            match resp {
                KeyboardInteractiveAuthResponse::Success => return Ok(true),
                KeyboardInteractiveAuthResponse::Failure { .. } => break,
                KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                    let answers = if prompts.is_empty() {
                        Vec::new()
                    } else if prompts.len() == 1 && !password.is_empty() && !password_sent && is_password_prompt(&prompts[0].prompt, user) {
                        password_sent = true;
                        vec![password.to_owned()]
                    } else if let Some(p) = prompter {
                        asked_user = true;
                        let req = KbdRequest {
                            name,
                            instructions,
                            prompts: prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect(),
                        };
                        match p(req).await {
                            Some(a) => a,
                            None => bail!("authentication cancelled by the user"),
                        }
                    } else if !password.is_empty() && !password_sent {
                        password_sent = true;
                        prompts.iter().map(|_| password.to_owned()).collect()
                    } else {
                        return Ok(false);
                    };
                    resp = handle.authenticate_keyboard_interactive_respond(answers).await?;
                }
            }
        }
        // Only retry when the user typed something that may have been wrong.
        if !asked_user {
            break;
        }
    }
    Ok(false)
}

#[cfg(unix)]
async fn auth_with_agent(handle: &mut Handle<Handler>, user: &str) -> Result<bool> {
    let Ok(mut agent) = AgentClient::connect_env().await else {
        return Ok(false);
    };
    // Servers commonly allow ~6 attempts; leave room for password auth.
    let identities: Vec<_> = agent.request_identities().await.unwrap_or_default().into_iter().take(4).collect();
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    for id in identities {
        if let AgentIdentity::PublicKey { key, .. } = id {
            if let Ok(res) = handle.authenticate_publickey_with(user, key, hash, &mut agent).await {
                if res.success() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
async fn auth_with_agent(_handle: &mut Handle<Handler>, _user: &str) -> Result<bool> {
    Ok(false)
}

impl SftpClient {
    pub async fn connect(cfg: &SftpConfig) -> Result<Self> {
        let known_hosts = cfg.known_hosts_path()?;
        let problem = Arc::new(Mutex::new(None));
        let handler = Handler {
            host: cfg.host.clone(),
            port: cfg.port,
            known_hosts,
            trust: cfg.trust_fingerprint.clone(),
            accept_new: cfg.accept_new_host_key,
            remember: cfg.remember_host_key,
            problem: problem.clone(),
            status: cfg.status.clone(),
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(600)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });

        cfg.status(&format!("Connecting to {}:{}...", cfg.host, cfg.port));
        let connected = tokio::time::timeout(
            cfg.timeout,
            client::connect(config, (cfg.host.as_str(), cfg.port), handler),
        )
        .await;
        let mut handle = match connected {
            Err(_) => bail!("connection to {}:{} timed out after {:?}", cfg.host, cfg.port, cfg.timeout),
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                if let Some(p) = problem.lock().unwrap().take() {
                    return Err(p.into());
                }
                return Err(anyhow!(e).context(format!("could not connect to {}:{}", cfg.host, cfg.port)));
            }
        };
        cfg.status("SSH connection established");

        let user = if cfg.user.is_empty() { std::env::var("USER").unwrap_or_default() } else { cfg.user.clone() };
        let mut tried = Vec::new();
        let mut ok = false;

        // 1. publickey: the site's key file, then ssh-agent, then ~/.ssh defaults.
        if let Some(path) = &cfg.key_file {
            let key = load_key(path, cfg.key_passphrase.as_deref())?;
            tried.push(format!("key {}", path.display()));
            ok = auth_with_key(&mut handle, &user, key).await?;
        }
        if !ok {
            tried.push("ssh-agent".into());
            ok = auth_with_agent(&mut handle, &user).await?;
        }
        if !ok && cfg.key_file.is_none() {
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                let path = home_dir()?.join(".ssh").join(name);
                // Encrypted default keys are skipped silently (no prompt for keys
                // the user did not choose); use the key file setting for those.
                if let Ok(key) = russh::keys::load_secret_key(&path, None) {
                    tried.push(format!("~/.ssh/{name}"));
                    if auth_with_key(&mut handle, &user, key).await? {
                        ok = true;
                        break;
                    }
                }
            }
        }
        // 2. password, 3. keyboard-interactive (password prompt or dialog).
        if !ok && !cfg.password.is_empty() {
            tried.push("password".into());
            ok = auth_with_password(&mut handle, &user, &cfg.password).await?;
        }
        if !ok && (!cfg.password.is_empty() || cfg.prompter.is_some()) && user != "anonymous" {
            tried.push("keyboard-interactive".into());
            ok = auth_keyboard_interactive(&mut handle, &user, &cfg.password, cfg.prompter.as_ref()).await?;
        }
        if !ok {
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            bail!("authentication failed for user {user:?} (tried: {})", tried.join(", "));
        }
        cfg.status(&format!("Authenticated as {user}"));

        let channel = handle.channel_open_session().await.context("could not open SSH channel")?;
        channel.request_subsystem(true, "sftp").await.context("server refused the SFTP subsystem")?;
        let sftp = SftpSession::new(channel.into_stream()).await.context("SFTP handshake failed")?;
        sftp.set_timeout(cfg.timeout.as_secs().max(10));
        let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into());
        let raw = async {
            let ch = handle.channel_open_session().await.ok()?;
            ch.request_subsystem(true, "sftp").await.ok()?;
            let raw = RawSftpSession::new(ch.into_stream());
            raw.set_timeout(cfg.timeout.as_secs().max(10));
            raw.init().await.ok()?;
            Some(raw)
        }
        .await;
        Ok(Self { handle, sftp, raw, home })
    }

    /// The user's start directory on the server.
    pub fn home(&self) -> &str {
        &self.home
    }

    /// Names, attributes and longnames of a directory.
    async fn read_dir_raw(&self, path: &str) -> Result<Vec<(String, FileAttributes, String)>> {
        if let Some(raw) = &self.raw {
            let h = raw.opendir(path).await.map_err(fx).with_context(|| format!("listing {path}"))?;
            let mut out = Vec::new();
            let res = loop {
                match raw.readdir(h.handle.clone()).await {
                    Ok(name) => out.extend(name.files.into_iter().map(|f| (f.filename, f.attrs, f.longname))),
                    Err(SftpError::Status(st)) if st.status_code == StatusCode::Eof => break Ok(()),
                    Err(e) => break Err(fx(e)),
                }
            };
            let _ = raw.close(h.handle.clone()).await;
            res.with_context(|| format!("listing {path}"))?;
            return Ok(out);
        }
        let dir = self.sftp.read_dir(path).await.map_err(fx).with_context(|| format!("listing {path}"))?;
        Ok(dir.map(|e| (e.file_name(), e.metadata(), String::new())).collect())
    }

    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for (name, attrs, longname) in self.read_dir_raw(path).await? {
            if name == "." || name == ".." || name.is_empty() || name.contains(['/', '\0']) {
                continue;
            }
            let ft = attrs.file_type();
            let (perm_str, owner) = match parse_longname(&longname) {
                Some((p, o)) => (p, o),
                None => (attrs.permissions.map(mode_string).unwrap_or_default(), owner_group(&attrs)),
            };
            let mut entry = DirEntry {
                size: attrs.size.filter(|s| *s <= i64::MAX as u64).map(|s| s as i64).unwrap_or(-1),
                permissions: perm_str,
                owner_group: owner,
                time: attrs
                    .mtime
                    .map(|t| DateTime::from_unix_secs(i64::from(t), Accuracy::Seconds))
                    .unwrap_or_default(),
                ..Default::default()
            };
            if ft.is_dir() {
                entry.flags |= FLAG_DIR;
                entry.size = -1;
            } else if ft.is_symlink() {
                let full = join(path, &name);
                entry.flags |= FLAG_LINK;
                entry.target = self.sftp.read_link(full.as_str()).await.ok();
                if let Ok(target) = self.sftp.metadata(full.as_str()).await {
                    if target.file_type().is_dir() {
                        entry.flags |= FLAG_DIR;
                        entry.size = -1;
                    } else if let Some(s) = target.size {
                        entry.size = s as i64;
                    }
                }
            }
            entry.name = name;
            out.push(entry);
        }
        Ok(out)
    }

    /// Size, time and type of one path; `None` if it does not exist.
    pub async fn stat(&self, path: &str) -> Result<Option<DirEntry>> {
        match self.sftp.metadata(path).await {
            Ok(a) => Ok(Some(DirEntry {
                name: path.rsplit('/').next().unwrap_or("").to_owned(),
                size: if a.file_type().is_dir() { -1 } else { a.size.map(|s| s as i64).unwrap_or(-1) },
                time: a.mtime.map(|t| DateTime::from_unix_secs(i64::from(t), Accuracy::Seconds)).unwrap_or_default(),
                flags: if a.file_type().is_dir() { FLAG_DIR } else { 0 },
                ..Default::default()
            })),
            Err(SftpError::Status(st)) if st.status_code == StatusCode::NoSuchFile => Ok(None),
            Err(e) => Err(fx(e)).with_context(|| format!("stat {path}")),
        }
    }

    /// Set the modification (and access) time.
    pub async fn set_mtime(&self, path: &str, time: &DateTime) -> Result<()> {
        let secs = time.unix_millis().ok_or_else(|| anyhow!("no time"))? / 1000;
        let secs = u32::try_from(secs).map_err(|_| anyhow!("time out of range for SFTP"))?;
        let mut attrs = FileAttributes::empty();
        attrs.atime = Some(secs);
        attrs.mtime = Some(secs);
        self.sftp.set_metadata(path, attrs).await.map_err(fx).with_context(|| format!("setting the time of {path}"))
    }

    pub async fn is_dir(&self, path: &str) -> Result<bool> {
        Ok(self.sftp.metadata(path).await.map(|m| m.file_type().is_dir()).unwrap_or(false))
    }

    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        path: &str,
        out: &mut W,
        progress: impl FnMut(u64),
    ) -> Result<u64> {
        self.download_from(path, out, 0, progress).await
    }

    /// Download from byte `offset` (resume). Returns the bytes read now.
    pub async fn download_from<W: AsyncWrite + Unpin>(
        &self,
        path: &str,
        out: &mut W,
        offset: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let mut f = self.sftp.open(path).await.map_err(fx).with_context(|| format!("opening {path}"))?;
        if offset > 0 {
            f.seek(std::io::SeekFrom::Start(offset)).await.with_context(|| format!("seeking in {path}"))?;
        }
        let mut buf = vec![0u8; 255 * 1024];
        let mut total = 0u64;
        loop {
            let n = match f.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if is_eof(&e) => break,
                Err(e) => return Err(e).with_context(|| format!("reading {path}")),
            };
            out.write_all(&buf[..n]).await?;
            total += n as u64;
            progress(total);
        }
        out.flush().await?;
        let _ = f.shutdown().await;
        Ok(total)
    }

    pub async fn upload<R: AsyncRead + Unpin>(
        &self,
        path: &str,
        input: &mut R,
        progress: impl FnMut(u64),
    ) -> Result<u64> {
        self.upload_from(path, input, 0, progress).await
    }

    /// Upload; with `offset > 0` the remote file is kept and written from
    /// that position (resume). `input` must already be at the same offset.
    pub async fn upload_from<R: AsyncRead + Unpin>(
        &self,
        path: &str,
        input: &mut R,
        offset: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let mut f = if offset == 0 {
            self.sftp.create(path).await.map_err(fx).with_context(|| format!("creating {path}"))?
        } else {
            let mut f = self
                .sftp
                .open_with_flags(path, OpenFlags::WRITE)
                .await
                .map_err(fx)
                .with_context(|| format!("opening {path}"))?;
            f.seek(std::io::SeekFrom::Start(offset)).await.with_context(|| format!("seeking in {path}"))?;
            f
        };
        let mut buf = vec![0u8; 255 * 1024];
        let mut total = 0u64;
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).await.with_context(|| format!("writing {path}"))?;
            total += n as u64;
            progress(total);
        }
        f.shutdown().await.with_context(|| format!("closing {path}"))?;
        Ok(total)
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.sftp.create_dir(path).await.map_err(fx).with_context(|| format!("creating directory {path}"))
    }

    pub async fn rmdir(&self, path: &str) -> Result<()> {
        self.sftp.remove_dir(path).await.map_err(fx).with_context(|| format!("removing directory {path}"))
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.sftp.remove_file(path).await.map_err(fx).with_context(|| format!("deleting {path}"))
    }

    /// Rename/move. With `overwrite`, an existing target is replaced
    /// atomically when the server supports `posix-rename@openssh.com`.
    pub async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        if overwrite && self.sftp.posix_rename(from, to).await.map_err(fx).with_context(|| format!("renaming {from} to {to}"))? {
            return Ok(());
        }
        self.sftp.rename(from, to).await.map_err(fx).with_context(|| format!("renaming {from} to {to}"))
    }

    /// `mode` is octal, e.g. "644".
    pub async fn chmod(&self, path: &str, mode: &str) -> Result<()> {
        let bits = u32::from_str_radix(mode.trim(), 8).map_err(|_| anyhow!("invalid mode {mode:?}, expected octal like 644"))?;
        let current = self.sftp.metadata(path).await.map_err(fx).with_context(|| format!("chmod {path}: reading the current mode"))?;
        let Some(cur_mode) = current.permissions else {
            bail!("chmod {path}: the server did not report the current mode");
        };
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some((cur_mode & 0o170000) | (bits & 0o7777));
        self.sftp.set_metadata(path, attrs).await.map_err(fx).with_context(|| format!("chmod {path}"))
    }

    /// Delete a directory tree (symbolic links are removed, not followed).
    pub async fn remove_recursive(&self, path: &str) -> Result<()> {
        let mut stack = vec![(path.to_owned(), false)];
        while let Some((dir, visited)) = stack.pop() {
            if visited {
                self.rmdir(&dir).await?;
                continue;
            }
            let entries = self.list(&dir).await?;
            stack.push((dir.clone(), true));
            for e in entries {
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

    pub async fn close(self) {
        if let Some(raw) = &self.raw {
            let _ = raw.close_session();
        }
        let _ = self.sftp.close().await;
        let _ = self.handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }
}

/// Default location of the user's private keys, for UI hints.
pub fn default_key_dir() -> Option<PathBuf> {
    home_dir().ok().map(|h| h.join(".ssh"))
}

/// Whether `path` looks like a readable private key file.
pub fn key_exists(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(mode_string(0o040755), "drwxr-xr-x");
        assert_eq!(mode_string(0o100644), "-rw-r--r--");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx");
        assert_eq!(mode_string(0o104755), "-rwsr-xr-x");
        assert_eq!(mode_string(0o041777), "drwxrwxrwt");
        assert_eq!(mode_string(0o100600), "-rw-------");
    }

    #[test]
    fn longnames() {
        assert_eq!(
            parse_longname("drwxr-xr-x    2 alice    staff        4096 Jan  1 00:00 docs"),
            Some(("drwxr-xr-x".into(), "alice staff".into()))
        );
        assert_eq!(parse_longname(""), None);
        assert_eq!(parse_longname("weird"), None);
    }

    #[test]
    fn joins() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/home/u", "a b"), "/home/u/a b");
    }
}

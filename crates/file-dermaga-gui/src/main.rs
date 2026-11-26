//! FileDermaga desktop client: Tauri backend.
//!
//! The browsing connection of a session is kept behind a mutex; each queued
//! transfer opens its own connection (FTP) or clones the client (S3), so
//! browsing stays responsive while files move.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use file_dermaga_core::{Accuracy, DirEntry};
use file_dermaga_ftp::{FtpClient, FtpConfig, LogKind, Logger, Security};
use file_dermaga_s3::{S3Client, S3Config, UploadOptions};
use file_dermaga_transfer::{basename, Backend, Event};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Semaphore;

/// Transfers running at the same time.
const MAX_PARALLEL_TRANSFERS: usize = 2;

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn err_chain(e: anyhow::Error) -> String {
    format!("{e:#}")
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Site {
    /// Stable id; keys the site's secrets in the OS keyring.
    id: String,
    name: String,
    /// ftp | ftpes | ftps | ftp-plain | s3
    protocol: String,
    host: String,
    port: Option<u16>,
    user: String,
    /// Never written to disk.
    #[serde(skip_serializing)]
    password: String,
    insecure: bool,
    // S3
    endpoint: String,
    region: String,
    profile: String,
    access_key: String,
    #[serde(skip_serializing)]
    secret_key: String,
    path_style: bool,
    storage_class: String,
    sse: String,
    remote_dir: String,
    local_dir: String,
    /// Keep the password / secret key in the OS keyring.
    save_password: bool,
    /// Output only: a secret for this site is in the keyring.
    password_saved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryDto {
    name: String,
    size: i64,
    is_dir: bool,
    is_link: bool,
    /// Milliseconds since the epoch.
    mtime: Option<i64>,
    /// d | h | m | s
    mtime_acc: &'static str,
    permissions: String,
    owner: String,
    target: Option<String>,
}

impl From<DirEntry> for EntryDto {
    fn from(e: DirEntry) -> Self {
        let is_dir = e.is_dir();
        let is_link = e.is_link();
        Self {
            mtime: e.time.unix_millis(),
            mtime_acc: match e.time.accuracy() {
                Accuracy::Days => "d",
                Accuracy::Hours => "h",
                Accuracy::Minutes => "m",
                _ => "s",
            },
            name: e.name,
            size: e.size,
            is_dir,
            is_link,
            permissions: e.permissions,
            owner: e.owner_group,
            target: e.target,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectInfo {
    session: u32,
    home: String,
    label: String,
    secure: bool,
    s3: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogLine {
    kind: &'static str,
    text: String,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct TransferEvent {
    id: u64,
    /// queued | active | progress | file-done | done | failed | cancelled
    state: &'static str,
    local: String,
    remote: String,
    done: u64,
    total: Option<u64>,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum Kind {
    Ftp(FtpConfig),
    S3(S3Client),
}

struct Session {
    label: String,
    kind: Kind,
    browse: tokio::sync::Mutex<Option<Backend>>,
}

impl Session {
    /// A separate connection for one transfer.
    async fn transfer_backend(&self) -> anyhow::Result<Backend> {
        Ok(match &self.kind {
            Kind::Ftp(cfg) => Backend::Ftp(FtpClient::connect(cfg).await?),
            Kind::S3(c) => Backend::S3(c.clone()),
        })
    }
}

struct AppState {
    sessions: Mutex<HashMap<u32, Arc<Session>>>,
    next_session: AtomicU32,
    transfers: Mutex<HashMap<u64, tauri::async_runtime::JoinHandle<()>>>,
    next_transfer: AtomicU64,
    slots: Arc<Semaphore>,
}

impl AppState {
    fn session(&self, id: u32) -> CmdResult<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned().ok_or_else(|| "Not connected".to_owned())
    }
}

fn log(app: &AppHandle, kind: &'static str, text: impl Into<String>) {
    let _ = app.emit("log", LogLine { kind, text: text.into() });
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Accepts `host`, `host:port` or `scheme://host[:port]` in the host field.
fn split_host(site: &mut Site) {
    if let Some((scheme, rest)) = site.host.split_once("://") {
        site.protocol = scheme.to_ascii_lowercase();
        site.host = rest.trim_end_matches('/').to_owned();
    }
    if site.protocol == "s3" {
        return;
    }
    if let Some((h, p)) = site.host.rsplit_once(':') {
        if let Ok(port) = p.parse() {
            if site.port.is_none() {
                site.port = Some(port);
            }
            site.host = h.to_owned();
        }
    }
}

// --- OS keyring -------------------------------------------------------------

const KEYRING_SERVICE: &str = "file-dermaga";
/// Keyring service and config directory used before the project was renamed.
const LEGACY_KEYRING_SERVICE: &str = "gojjira";
const LEGACY_CONFIG_DIR: &str = "app.gojjira.client";
const SECRET_PASSWORD: &str = "password";
const SECRET_S3: &str = "secret";

fn keyring_entry(id: &str, kind: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("{id}/{kind}"))
}

fn secret_get(id: &str, kind: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    if let Ok(v) = keyring_entry(id, kind).and_then(|e| e.get_password()) {
        return Some(v);
    }
    // Move a secret saved under the old name to the new service.
    let legacy = keyring::Entry::new(LEGACY_KEYRING_SERVICE, &format!("{id}/{kind}")).ok()?;
    let v = legacy.get_password().ok()?;
    if secret_set(id, kind, &v).is_ok() {
        let _ = legacy.delete_credential();
    }
    Some(v)
}

/// Copy the site list saved under the old app name, once.
fn migrate_legacy_config(app: &AppHandle) {
    let Ok(new_dir) = app.path().app_config_dir() else { return };
    let new_file = new_dir.join("sites.json");
    let Some(old_file) = new_dir.parent().map(|p| p.join(LEGACY_CONFIG_DIR).join("sites.json")) else { return };
    if new_file.exists() || !old_file.exists() {
        return;
    }
    if std::fs::create_dir_all(&new_dir).is_ok() && std::fs::copy(&old_file, &new_file).is_ok() {
        eprintln!("Imported saved sites from {}", old_file.display());
    }
}

fn secret_set(id: &str, kind: &str, value: &str) -> CmdResult<()> {
    keyring_entry(id, kind)
        .and_then(|e| e.set_password(value))
        .map_err(|e| format!("Could not save the secret in the system keyring: {e}"))
}

fn secret_delete(id: &str, kind: &str) {
    if let Ok(e) = keyring_entry(id, kind) {
        let _ = e.delete_credential();
    }
    if let Ok(e) = keyring::Entry::new(LEGACY_KEYRING_SERVICE, &format!("{id}/{kind}")) {
        let _ = e.delete_credential();
    }
}

/// Fill in a saved password / secret key the UI left empty.
fn fill_secrets(site: &mut Site) {
    if !site.save_password || site.id.is_empty() {
        return;
    }
    if site.password.is_empty() {
        if let Some(p) = secret_get(&site.id, SECRET_PASSWORD) {
            site.password = p;
        }
    }
    if site.secret_key.is_empty() {
        if let Some(p) = secret_get(&site.id, SECRET_S3) {
            site.secret_key = p;
        }
    }
}

/// Keyring access talks D-Bus; keep it off the async runtime threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)
}

// --- Connection setup ----------------------------------------------------------

fn opt(s: &str) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.trim().to_owned())
}

fn s3_config_of(site: &Site) -> S3Config {
    S3Config {
        endpoint: opt(&site.endpoint),
        region: opt(&site.region),
        profile: opt(&site.profile),
        access_key: opt(&site.access_key),
        secret_key: (!site.secret_key.is_empty()).then(|| site.secret_key.clone()),
        session_token: None,
        path_style: site.path_style,
        upload: UploadOptions {
            storage_class: opt(&site.storage_class),
            sse: opt(&site.sse),
            ..Default::default()
        },
    }
}

fn security_of(protocol: &str) -> (Security, u16) {
    match protocol {
        "ftpes" => (Security::ExplicitTls, 21),
        "ftps" => (Security::ImplicitTls, 990),
        "ftp-plain" => (Security::Plain, 21),
        _ => (Security::TryTls, 21),
    }
}

fn ftp_config_of(site: &Site, logger: Option<Logger>) -> CmdResult<FtpConfig> {
    if site.host.is_empty() {
        return Err("Host is empty".into());
    }
    let (security, default_port) = security_of(&site.protocol);
    let mut cfg = FtpConfig::new(site.host.clone());
    cfg.port = site.port.unwrap_or(default_port);
    cfg.security = security;
    cfg.user = site.user.clone();
    cfg.password = site.password.clone();
    cfg.accept_invalid_certs = site.insecure;
    cfg.logger = logger;
    Ok(cfg)
}

fn log_kind(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Status => "status",
        LogKind::Command => "command",
        LogKind::Response => "response",
        LogKind::Error => "error",
    }
}

fn s3_where(cfg: &S3Config) -> String {
    cfg.endpoint.clone().unwrap_or_else(|| "Amazon S3".into())
}

/// Check S3 credentials. Keys restricted to one bucket often may not call
/// ListBuckets, so a configured default bucket is listed instead.
async fn s3_probe(client: &S3Client, remote_dir: &str) -> anyhow::Result<String> {
    let dir = remote_dir.trim_matches('/');
    if dir.is_empty() {
        let buckets = client.list_buckets().await?;
        let names: Vec<_> = buckets.iter().take(5).map(|b| b.name.as_str()).collect();
        let more = if buckets.len() > 5 { ", …" } else { "" };
        Ok(format!("{} bucket(s) visible: {}{more}", buckets.len(), names.join(", ")))
    } else {
        let entries = client.list(dir).await?;
        Ok(format!("/{dir}: {} item(s)", entries.len()))
    }
}

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>, mut site: Site) -> CmdResult<ConnectInfo> {
    split_host(&mut site);
    let mut site = blocking(move || {
        fill_secrets(&mut site);
        site
    })
    .await?;
    let id = state.next_session.fetch_add(1, Ordering::Relaxed) + 1;

    let (session, home, secure) = if site.protocol == "s3" {
        let cfg = s3_config_of(&site);
        let where_ = s3_where(&cfg);
        log(&app, "status", format!("Connecting to {where_}..."));
        let client = S3Client::connect(&cfg).await.map_err(err_chain)?;
        log(&app, "status", "Checking credentials...");
        match s3_probe(&client, &site.remote_dir).await {
            Ok(m) => log(&app, "status", m),
            Err(e) => {
                let m = err_chain(e);
                log(&app, "error", m.clone());
                return Err(m);
            }
        }
        log(&app, "status", format!("Connected (region {})", client.region()));
        let session = Session {
            label: format!("s3://{where_}"),
            kind: Kind::S3(client.clone()),
            browse: tokio::sync::Mutex::new(Some(Backend::S3(client))),
        };
        let home = if site.remote_dir.is_empty() { "/".into() } else { site.remote_dir.clone() };
        (session, home, true)
    } else {
        let app2 = app.clone();
        let logger = Logger(Arc::new(move |kind, text| log(&app2, log_kind(kind), text)));
        let cfg = ftp_config_of(&site, Some(logger))?;
        log(&app, "status", format!("Resolving address of {}", cfg.host));
        let mut client = FtpClient::connect(&cfg).await.map_err(|e| {
            let m = e.to_string();
            log(&app, "error", format!("Could not connect to server: {m}"));
            m
        })?;
        let secure = client.is_secure();
        if !secure {
            log(&app, "error", "Connection is not encrypted (server does not support TLS)");
        }
        let home = if site.remote_dir.is_empty() { client.pwd().await.map_err(err)? } else { site.remote_dir.clone() };
        // Transfer connections should not flood the message log.
        let mut transfer_cfg = cfg.clone();
        transfer_cfg.logger = None;
        let label = format!("{}://{}@{}", site.protocol.trim_end_matches("-plain"), cfg.user, cfg.host);
        site.password.clear();
        let session = Session {
            label,
            kind: Kind::Ftp(transfer_cfg),
            browse: tokio::sync::Mutex::new(Some(Backend::Ftp(client))),
        };
        (session, home, secure)
    };

    let label = session.label.clone();
    let s3 = matches!(session.kind, Kind::S3(_));
    state.sessions.lock().unwrap().insert(id, Arc::new(session));
    Ok(ConnectInfo { session: id, home, label, secure, s3 })
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct TestReport {
    ok: bool,
    summary: String,
    details: Vec<String>,
    transcript: Vec<String>,
    elapsed_ms: u64,
    /// The failure was an untrusted TLS certificate.
    cert_error: bool,
}

const TEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Try a site's settings without opening a session (like a database tool's
/// "Test connection"): connect, authenticate, read the start directory.
#[tauri::command]
async fn test_connection(mut site: Site) -> CmdResult<TestReport> {
    split_host(&mut site);
    let site = blocking(move || {
        fill_secrets(&mut site);
        site
    })
    .await?;
    let t0 = Instant::now();
    let mut r = TestReport::default();

    if site.protocol == "s3" {
        let cfg = s3_config_of(&site);
        r.details.push(format!("Endpoint: {}", s3_where(&cfg)));
        if cfg.access_key.is_some() && cfg.secret_key.is_none() {
            r.summary = "No secret key entered or saved for this access key.".into();
            return Ok(r);
        }
        let res = tokio::time::timeout(TEST_TIMEOUT, async {
            let client = S3Client::connect(&cfg).await?;
            let region = client.region().to_owned();
            let probe = s3_probe(&client, &site.remote_dir).await?;
            Ok::<_, anyhow::Error>((region, probe))
        })
        .await;
        match res {
            Ok(Ok((region, probe))) => {
                r.ok = true;
                r.details.push(format!("Region: {region}"));
                r.details.push(probe);
                r.summary = "Connected and authenticated.".into();
            }
            Ok(Err(e)) => r.summary = err_chain(e),
            Err(_) => r.summary = format!("No answer within {} seconds.", TEST_TIMEOUT.as_secs()),
        }
    } else {
        if site.user.is_empty() {
            r.details.push("User: anonymous".into());
        } else if site.user != "anonymous" && site.password.is_empty() {
            r.summary = "No password entered or saved.".into();
            return Ok(r);
        }
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let logger = Logger(Arc::new(move |kind, text| {
            sink.lock().unwrap().push(format!("{:<9} {text}", format!("{}:", kind.label())));
        }));
        let mut cfg = ftp_config_of(&site, Some(logger))?;
        cfg.timeout = TEST_TIMEOUT;
        r.details.push(format!("Server: {}:{}", cfg.host, cfg.port));
        match FtpClient::connect(&cfg).await {
            Err(e) => {
                let m = e.to_string();
                r.cert_error = matches!(e, file_dermaga_ftp::FtpError::Tls(_)) && m.contains("certificate");
                r.summary = m;
            }
            Ok(mut c) => {
                r.details.push(
                    match (c.is_secure(), cfg.security) {
                        (true, Security::ImplicitTls) => "Encryption: TLS (implicit)",
                        (true, _) => "Encryption: TLS (explicit, AUTH TLS)",
                        (false, _) => "Encryption: none — the connection is NOT encrypted",
                    }
                    .into(),
                );
                let f = c.features();
                let feats: Vec<&str> = [("MLSD", f.mlsd), ("UTF8", f.utf8), ("SIZE", f.size), ("MDTM", f.mdtm), ("REST", f.rest_stream)]
                    .into_iter()
                    .filter_map(|(n, on)| on.then_some(n))
                    .collect();
                r.details.push(format!("Server features: {}", if feats.is_empty() { "none advertised".into() } else { feats.join(", ") }));
                let start = if site.remote_dir.is_empty() { c.pwd().await.unwrap_or_else(|_| "/".into()) } else { site.remote_dir.clone() };
                match c.list(&start).await {
                    Ok(l) => {
                        r.ok = true;
                        r.details.push(format!("Directory {start}: {} item(s), listing transfer works", l.len()));
                        r.summary = "Connected, logged in and listed the start directory.".into();
                    }
                    Err(e) => r.summary = format!("Logged in, but listing {start} failed: {e}"),
                }
                let _ = c.quit().await;
            }
        }
        r.transcript = std::mem::take(&mut *lines.lock().unwrap());
    }
    r.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(r)
}

/// Save a secret that was typed in at connect time.
#[tauri::command]
async fn site_secret_set(id: String, kind: String, value: String) -> CmdResult<()> {
    if id.is_empty() || !(kind == SECRET_PASSWORD || kind == SECRET_S3) {
        return Err("invalid site".into());
    }
    blocking(move || secret_set(&id, &kind, &value)).await?
}

#[tauri::command]
async fn disconnect(app: AppHandle, state: State<'_, AppState>, session: u32) -> CmdResult<()> {
    let s = state.sessions.lock().unwrap().remove(&session);
    if let Some(s) = s {
        if let Some(b) = s.browse.lock().await.take() {
            b.close().await;
        }
        log(&app, "status", "Disconnected from server");
    }
    Ok(())
}

/// Run an operation on the session's browsing connection.
async fn with_browse<T>(
    state: &State<'_, AppState>,
    session: u32,
    f: impl AsyncFnOnce(&mut Backend) -> anyhow::Result<T>,
) -> CmdResult<T> {
    let s = state.session(session)?;
    let mut guard = s.browse.lock().await;
    let b = guard.as_mut().ok_or("Not connected")?;
    f(b).await.map_err(err_chain)
}

fn s3_path(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// The backend uses `bucket/key` for S3; the UI always shows `/bucket/key`.
fn backend_path(s: &Session, path: &str) -> String {
    match s.kind {
        Kind::S3(_) => s3_path(path).to_owned(),
        Kind::Ftp(_) => path.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Remote commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn remote_list(app: AppHandle, state: State<'_, AppState>, session: u32, path: String) -> CmdResult<Vec<EntryDto>> {
    let s = state.session(session)?;
    let p = backend_path(&s, &path);
    log(&app, "status", format!("Retrieving directory listing of \"{path}\"..."));
    let res = with_browse(&state, session, async |b| b.list(&p).await).await;
    match res {
        Ok(entries) => {
            log(&app, "status", format!("Directory listing of \"{path}\" successful"));
            Ok(entries.into_iter().map(Into::into).collect())
        }
        Err(e) => {
            log(&app, "error", format!("Failed to retrieve directory listing: {e}"));
            Err(e)
        }
    }
}

#[tauri::command]
async fn remote_mkdir(app: AppHandle, state: State<'_, AppState>, session: u32, path: String) -> CmdResult<()> {
    let s = state.session(session)?;
    let p = backend_path(&s, &path);
    let r = with_browse(&state, session, async |b| b.mkdir(&p).await).await;
    match &r {
        Ok(()) => log(&app, "status", format!("Created directory \"{path}\"")),
        Err(e) => log(&app, "error", e.clone()),
    }
    r
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathItem {
    path: String,
    is_dir: bool,
}

#[tauri::command]
async fn remote_delete(app: AppHandle, state: State<'_, AppState>, session: u32, items: Vec<PathItem>) -> CmdResult<()> {
    let s = state.session(session)?;
    for item in items {
        let p = backend_path(&s, &item.path);
        let r = with_browse(&state, session, async |b| {
            if item.is_dir {
                b.rmdir(&p, true).await
            } else {
                b.delete(&p).await
            }
        })
        .await;
        match r {
            Ok(()) => log(&app, "status", format!("Deleted \"{}\"", item.path)),
            Err(e) => {
                log(&app, "error", e.clone());
                return Err(e);
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn remote_rename(app: AppHandle, state: State<'_, AppState>, session: u32, from: String, to: String) -> CmdResult<()> {
    let s = state.session(session)?;
    let (f, t) = (backend_path(&s, &from), backend_path(&s, &to));
    let r = with_browse(&state, session, async |b| b.rename(&f, &t).await).await;
    match &r {
        Ok(()) => log(&app, "status", format!("Renamed \"{from}\" to \"{to}\"")),
        Err(e) => log(&app, "error", e.clone()),
    }
    r
}

#[tauri::command]
async fn remote_presign(state: State<'_, AppState>, session: u32, path: String, expires: u64) -> CmdResult<String> {
    let s = state.session(session)?;
    match &s.kind {
        Kind::S3(c) => c.presign(s3_path(&path), Duration::from_secs(expires)).await.map_err(err_chain),
        Kind::Ftp(_) => Err("Pre-signed URLs are only available for S3".into()),
    }
}

// ---------------------------------------------------------------------------
// Local commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn local_home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

#[tauri::command]
async fn local_list(path: String) -> CmdResult<Vec<EntryDto>> {
    let mut rd = tokio::fs::read_dir(&path).await.map_err(|e| format!("{path}: {e}"))?;
    let mut out = Vec::new();
    while let Some(de) = rd.next_entry().await.map_err(err)? {
        let name = de.file_name().to_string_lossy().into_owned();
        let link = tokio::fs::symlink_metadata(de.path()).await.ok();
        let is_link = link.as_ref().is_some_and(|m| m.file_type().is_symlink());
        // Follow links for type/size; broken links fall back to the link itself.
        let meta = match tokio::fs::metadata(de.path()).await {
            Ok(m) => m,
            Err(_) => match link {
                Some(m) => m,
                None => continue,
            },
        };
        let target = if is_link {
            tokio::fs::read_link(de.path()).await.ok().map(|p| p.display().to_string())
        } else {
            None
        };
        out.push(EntryDto {
            name,
            size: if meta.is_dir() { -1 } else { meta.len() as i64 },
            is_dir: meta.is_dir(),
            is_link,
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
            mtime_acc: "s",
            permissions: permissions_string(&meta),
            owner: String::new(),
            target,
        });
    }
    Ok(out)
}

#[cfg(unix)]
fn permissions_string(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let mut s = String::with_capacity(10);
    s.push(if meta.is_dir() { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(not(unix))]
fn permissions_string(_meta: &std::fs::Metadata) -> String {
    String::new()
}

#[tauri::command]
async fn local_mkdir(path: String) -> CmdResult<()> {
    tokio::fs::create_dir(&path).await.map_err(|e| format!("{path}: {e}"))
}

#[tauri::command]
async fn local_delete(items: Vec<PathItem>) -> CmdResult<()> {
    for item in items {
        let r = if item.is_dir {
            tokio::fs::remove_dir_all(&item.path).await
        } else {
            tokio::fs::remove_file(&item.path).await
        };
        r.map_err(|e| format!("{}: {e}", item.path))?;
    }
    Ok(())
}

#[tauri::command]
async fn local_rename(from: String, to: String) -> CmdResult<()> {
    if tokio::fs::try_exists(&to).await.unwrap_or(false) {
        return Err(format!("{to} already exists"));
    }
    tokio::fs::rename(&from, &to).await.map_err(|e| format!("{from}: {e}"))
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferRequest {
    session: u32,
    /// "upload" | "download"
    direction: String,
    local: String,
    remote: String,
    is_dir: bool,
    size: Option<u64>,
}

#[tauri::command]
fn transfer_start(app: AppHandle, state: State<'_, AppState>, req: TransferRequest) -> CmdResult<u64> {
    let session = state.session(req.session)?;
    let id = state.next_transfer.fetch_add(1, Ordering::Relaxed) + 1;
    let slots = state.slots.clone();

    let base = TransferEvent {
        id,
        local: req.local.clone(),
        remote: req.remote.clone(),
        total: req.size,
        ..Default::default()
    };
    let _ = app.emit("transfer", TransferEvent { state: "queued", ..base.clone() });

    let app2 = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let _permit = slots.acquire_owned().await;
        let emit = |e: TransferEvent| {
            let _ = app2.emit("transfer", e);
        };
        emit(TransferEvent { state: "active", ..base.clone() });

        let result = run_transfer(&session, &req, &mut |ev| match ev {
            TransferUpdate::Progress(done, total) => emit(TransferEvent {
                state: "progress",
                done,
                total,
                ..base.clone()
            }),
            TransferUpdate::File { local, remote, total } => emit(TransferEvent {
                state: "active",
                local,
                remote,
                total,
                ..base.clone()
            }),
            TransferUpdate::FileFailed { path, error } => {
                log(&app2, "error", format!("{path}: {error}"));
                emit(TransferEvent { state: "file-failed", local: path, error: Some(error), ..base.clone() })
            }
        })
        .await;

        match result {
            Ok(bytes) => {
                log(&app2, "status", format!("File transfer successful, transferred {bytes} bytes"));
                emit(TransferEvent { state: "done", done: bytes, ..base.clone() });
            }
            Err(e) => {
                let msg = err_chain(e);
                log(&app2, "error", format!("File transfer failed: {msg}"));
                emit(TransferEvent { state: "failed", error: Some(msg), ..base.clone() });
            }
        }
        if let Some(state) = app2.try_state::<AppState>() {
            state.transfers.lock().unwrap().remove(&id);
        }
    });
    state.transfers.lock().unwrap().insert(id, handle);
    Ok(id)
}

enum TransferUpdate {
    Progress(u64, Option<u64>),
    File { local: String, remote: String, total: Option<u64> },
    FileFailed { path: String, error: String },
}

async fn run_transfer(
    session: &Session,
    req: &TransferRequest,
    update: &mut (dyn FnMut(TransferUpdate) + Send),
) -> anyhow::Result<u64> {
    let mut backend = session.transfer_backend().await?;
    let remote = backend_path(session, &req.remote);
    let local = PathBuf::from(&req.local);

    // Throttle progress events to ~10 per second.
    let mut last = Instant::now() - Duration::from_secs(1);
    let total_hint = req.size;
    let mut progress = |done: u64, total: Option<u64>, update: &mut (dyn FnMut(TransferUpdate) + Send)| {
        if last.elapsed() >= Duration::from_millis(100) {
            last = Instant::now();
            update(TransferUpdate::Progress(done, total));
        }
    };

    let res = if req.is_dir {
        let mut bytes = 0u64;
        let mut current_total = None;
        let mut file_bytes = 0u64;
        let mut handler = |ev: Event| match ev {
            Event::FileStart { remote, local, size } => {
                current_total = size;
                file_bytes = 0;
                update(TransferUpdate::File { local: local.display().to_string(), remote, total: size });
            }
            Event::Progress { done } => {
                file_bytes = done;
                progress(done, current_total, update);
            }
            Event::FileDone { bytes: n } => bytes += n,
            Event::Skipped { .. } => {}
            Event::Failed { path, error } => update(TransferUpdate::FileFailed { path, error }),
        };
        let r = if req.direction == "upload" {
            backend.upload_tree(&local, &remote, &mut handler).await
        } else {
            backend.download_tree(&remote, &local, &mut handler).await
        };
        let _ = file_bytes;
        r.map(|()| bytes)
    } else if req.direction == "upload" {
        backend.upload(&local, &remote, &mut |d| progress(d, total_hint, update)).await
    } else {
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        backend.download(&remote, &local, &mut |d| progress(d, total_hint, update)).await
    };
    backend.close().await;
    res
}

#[tauri::command]
fn transfer_cancel(app: AppHandle, state: State<'_, AppState>, id: u64) -> CmdResult<()> {
    if let Some(h) = state.transfers.lock().unwrap().remove(&id) {
        h.abort();
        let _ = app.emit("transfer", TransferEvent { id, state: "cancelled", ..Default::default() });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Site manager
// ---------------------------------------------------------------------------

fn sites_file(app: &AppHandle) -> CmdResult<PathBuf> {
    let dir = app.path().app_config_dir().map_err(err)?;
    Ok(dir.join("sites.json"))
}

fn new_site_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{:016x}", h.finish())
}

fn read_sites(file: &Path) -> CmdResult<Vec<Site>> {
    match std::fs::read(file) {
        Ok(data) => serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

fn mark_saved(sites: &mut [Site]) {
    for s in sites {
        let kind = if s.protocol == "s3" { SECRET_S3 } else { SECRET_PASSWORD };
        s.password_saved = s.save_password && secret_get(&s.id, kind).is_some();
    }
}

#[tauri::command]
async fn sites_load(app: AppHandle) -> CmdResult<Vec<Site>> {
    let f = sites_file(&app)?;
    blocking(move || {
        let mut sites = read_sites(&f)?;
        for s in &mut sites {
            if s.id.is_empty() {
                s.id = new_site_id();
            }
        }
        mark_saved(&mut sites);
        Ok(sites)
    })
    .await?
}

/// Write the site list. Passwords and secret keys go to the OS keyring
/// (never to the JSON file); secrets of removed sites are deleted.
#[tauri::command]
async fn sites_save(app: AppHandle, sites: Vec<Site>) -> CmdResult<Vec<Site>> {
    let f = sites_file(&app)?;
    blocking(move || {
        let mut sites = sites;
        let old = read_sites(&f).unwrap_or_default();
        for s in &mut sites {
            if s.id.is_empty() {
                s.id = new_site_id();
            }
            if s.save_password {
                if !s.password.is_empty() {
                    secret_set(&s.id, SECRET_PASSWORD, &s.password)?;
                }
                if !s.secret_key.is_empty() {
                    secret_set(&s.id, SECRET_S3, &s.secret_key)?;
                }
            } else {
                secret_delete(&s.id, SECRET_PASSWORD);
                secret_delete(&s.id, SECRET_S3);
            }
        }
        for o in old.iter().filter(|o| !o.id.is_empty() && !sites.iter().any(|s| s.id == o.id)) {
            secret_delete(&o.id, SECRET_PASSWORD);
            secret_delete(&o.id, SECRET_S3);
        }
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir).map_err(err)?;
        }
        let data = serde_json::to_vec_pretty(&sites).map_err(err)?;
        let tmp = f.with_extension("json.tmp");
        std::fs::write(&tmp, data).map_err(err)?;
        std::fs::rename(&tmp, &f).map_err(err)?;
        mark_saved(&mut sites);
        Ok(sites)
    })
    .await?
}

#[tauri::command]
fn path_join(base: String, name: String) -> String {
    Path::new(&base).join(name).display().to_string()
}

#[tauri::command]
fn path_basename(path: String) -> String {
    basename(&path).to_owned()
}

#[cfg(debug_assertions)]
#[tauri::command]
fn selftest_report(app: AppHandle, ok: bool, report: String) {
    println!("{report}");
    app.exit(if ok { 0 } else { 1 });
}

#[cfg(not(debug_assertions))]
#[tauri::command]
fn selftest_report() {}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU32::new(0),
            transfers: Mutex::new(HashMap::new()),
            next_transfer: AtomicU64::new(0),
            slots: Arc::new(Semaphore::new(MAX_PARALLEL_TRANSFERS)),
        })
        .setup(|_app| {
            migrate_legacy_config(_app.handle());
            // Development aid: FILE_DERMAGA_GUI_SELFTEST=<file.js> runs a UI test
            // script inside the real webview (debug builds only).
            #[cfg(debug_assertions)]
            if let Ok(script) = std::env::var("FILE_DERMAGA_GUI_SELFTEST") {
                let js = std::fs::read_to_string(&script)?;
                let win = _app.get_webview_window("main").ok_or("no main window")?;
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let _ = win.eval(&js);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            remote_list,
            remote_mkdir,
            remote_delete,
            remote_rename,
            remote_presign,
            local_home,
            local_list,
            local_mkdir,
            local_delete,
            local_rename,
            transfer_start,
            transfer_cancel,
            sites_load,
            sites_save,
            test_connection,
            site_secret_set,
            path_join,
            path_basename,
            selftest_report,
        ])
        .run(tauri::generate_context!())
        .expect("error while running FileDermaga");
}

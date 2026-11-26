//! `file-dermaga`: command line FTP / FTPS / S3 client.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use file_dermaga_core::DirEntry;
use file_dermaga_ftp::{FtpClient, FtpConfig, Security};
use file_dermaga_transfer::{always, basename, join, parent, safe_name, Backend, Decision, Event, ExistsAction, Outcome, TransferOpts};
use file_dermaga_s3::{S3Client, S3Config, UploadOptions};
use file_dermaga_sftp::{KbdRequest, SftpClient, SftpConfig};
use file_dermaga_webdav::{WebdavClient, WebdavConfig};
use percent_encoding::percent_decode_str;

/// Release builds get the git tag through `FILE_DERMAGA_VERSION` (set by
/// .github/workflows/release.yml); local builds use the Cargo.toml version.
const APP_VERSION: &str = match option_env!("FILE_DERMAGA_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "file-dermaga", version = APP_VERSION, about = "FTP, FTPS and S3 client")]
#[command(after_help = "\
URLs:
  ftp://[user[:pass]@]host[:port]/path    FTP, upgrades to TLS when offered
  ftpes://user@host/path                  FTP with required explicit TLS
  ftps://user@host/path                   FTP with implicit TLS (port 990)
  sftp://[user[:pass]@]host[:port]/path   SFTP (SSH); keys from ssh-agent or ~/.ssh
  davs://[user@]host[:port]/path          WebDAV over HTTPS (dav:// for plain HTTP)
  s3://bucket/key                         Amazon S3 or S3-compatible storage

S3 credentials come from --access-key/--secret-key, the AWS_* environment
variables, or ~/.aws (see --profile).")]
struct Cli {
    #[command(flatten)]
    opts: Opts,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Opts {
    /// FTP password (otherwise taken from the URL or asked interactively)
    #[arg(long, env = "FILE_DERMAGA_PASSWORD", hide_env_values = true, global = true)]
    password: Option<String>,
    /// Never use TLS for ftp:// URLs
    #[arg(long, global = true)]
    plain: bool,
    /// SFTP: private key file (default: ssh-agent, then ~/.ssh/id_*)
    #[arg(short = 'i', long, global = true, help_heading = "SFTP")]
    key: Option<std::path::PathBuf>,
    /// SFTP: passphrase of the private key
    #[arg(long, env = "FILE_DERMAGA_KEY_PASSPHRASE", hide_env_values = true, global = true, help_heading = "SFTP")]
    key_passphrase: Option<String>,
    /// SFTP: trust and remember an unknown server host key
    #[arg(long, global = true, help_heading = "SFTP")]
    accept_new_host_key: bool,
    /// Accept invalid or self-signed TLS certificates
    #[arg(short = 'k', long, global = true)]
    insecure: bool,
    /// Show the FTP command/response log
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Network timeout in seconds
    #[arg(long, default_value_t = 20, global = true)]
    timeout: u64,

    /// When the target file exists: overwrite, newer, size, size-or-newer, resume or skip
    #[arg(long, default_value = "overwrite", global = true, help_heading = "Transfers")]
    if_exists: String,
    /// Resume interrupted transfers (same as --if-exists resume)
    #[arg(short = 'c', long = "continue", global = true, help_heading = "Transfers")]
    resume: bool,
    /// Speed limit in KiB/s for downloads and uploads (0 = unlimited)
    #[arg(long, default_value_t = 0, global = true, help_heading = "Transfers")]
    limit_rate: u64,
    /// Keep the source file's modification time
    #[arg(short = 'p', long, global = true, help_heading = "Transfers")]
    preserve_time: bool,

    /// S3 endpoint for compatible services (MinIO, Wasabi, R2, ...)
    #[arg(long, env = "FILE_DERMAGA_S3_ENDPOINT", global = true, help_heading = "S3")]
    endpoint: Option<String>,
    /// S3 region
    #[arg(long, global = true, help_heading = "S3")]
    region: Option<String>,
    /// Profile from ~/.aws/config
    #[arg(long, global = true, help_heading = "S3")]
    profile: Option<String>,
    #[arg(long, global = true, help_heading = "S3")]
    access_key: Option<String>,
    #[arg(long, global = true, hide_env_values = true, help_heading = "S3")]
    secret_key: Option<String>,
    /// Use path-style addressing (endpoint/bucket/key)
    #[arg(long, global = true, help_heading = "S3")]
    path_style: bool,
    /// Storage class for uploads, e.g. STANDARD_IA, GLACIER_IR
    #[arg(long, global = true, help_heading = "S3")]
    storage_class: Option<String>,
    /// Server-side encryption for uploads: AES256 or aws:kms
    #[arg(long, global = true, help_heading = "S3")]
    sse: Option<String>,
    /// KMS key id for --sse aws:kms
    #[arg(long, global = true, help_heading = "S3")]
    sse_kms_key_id: Option<String>,
    /// Multipart part size in MiB
    #[arg(long, default_value_t = 16, global = true, help_heading = "S3")]
    part_size: u64,
    /// Parts uploaded in parallel
    #[arg(long, default_value_t = 4, global = true, help_heading = "S3")]
    concurrency: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// List a directory or bucket
    Ls {
        url: String,
        /// Long format: permissions, owner, size and date
        #[arg(short)]
        l: bool,
    },
    /// Download a file (or a directory with -r)
    Get {
        url: String,
        local: Option<PathBuf>,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Upload a file (or a directory with -r)
    Put {
        local: PathBuf,
        url: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Create a directory (S3: a bucket or folder)
    Mkdir { url: String },
    /// Delete a file (or a whole directory with -r)
    Rm {
        url: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Delete an empty directory (S3: an empty bucket or folder)
    Rmdir { url: String },
    /// Rename or move; TO is a path on the same server, relative to the source directory
    Mv {
        url: String,
        to: String,
        /// Replace an existing target
        #[arg(short, long)]
        force: bool,
    },
    /// Change permissions (FTP SITE CHMOD)
    Chmod { mode: String, url: String },
    /// Print a pre-signed download URL (S3)
    Url {
        url: String,
        /// Validity in seconds
        #[arg(long, default_value_t = 3600)]
        expires: u64,
    },
}

impl Cmd {
    fn url(&self) -> &str {
        match self {
            Cmd::Ls { url, .. }
            | Cmd::Get { url, .. }
            | Cmd::Put { url, .. }
            | Cmd::Mkdir { url }
            | Cmd::Rm { url, .. }
            | Cmd::Rmdir { url }
            | Cmd::Mv { url, .. }
            | Cmd::Chmod { url, .. }
            | Cmd::Url { url, .. } => url,
        }
    }
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

enum Target {
    Ftp(FtpConfig, String),
    S3(String),
    Sftp(SftpConfig, String),
    Webdav(WebdavConfig, String),
}

fn ask_password(user: &str, host: &str) -> Result<String> {
    Ok(rpassword::prompt_password(format!("Password for {user}@{host}: "))?)
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn parse_target(s: &str, opts: &Opts) -> Result<Target> {
    if let Some(rest) = s.strip_prefix("s3://") {
        return Ok(Target::S3(rest.to_owned()));
    }
    let url = url::Url::parse(s).with_context(|| format!("invalid URL: {s}"))?;
    let path = decode(url.path());
    let path = if path.is_empty() { "/".to_owned() } else { path };
    let url_password = url.password().map(decode).or_else(|| opts.password.clone());

    if url.scheme() == "sftp" {
        let host = url.host_str().context("URL has no host")?.trim_start_matches('[').trim_end_matches(']');
        let mut cfg = SftpConfig::new(host);
        cfg.port = url.port().unwrap_or(22);
        cfg.user = decode(url.username());
        cfg.password = url_password.unwrap_or_default();
        cfg.key_file = opts.key.clone();
        cfg.key_passphrase = opts.key_passphrase.clone();
        cfg.accept_new_host_key = opts.accept_new_host_key;
        cfg.timeout = Duration::from_secs(opts.timeout);
        if opts.verbose {
            cfg.status = Some(std::sync::Arc::new(|m: &str| eprintln!("   Status: {m}")));
        }
        if std::io::stdin().is_terminal() {
            cfg.prompter = Some(std::sync::Arc::new(|req: KbdRequest| {
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || {
                        if !req.instructions.is_empty() {
                            eprintln!("{}", req.instructions);
                        }
                        let mut answers = Vec::new();
                        for (text, echo) in req.prompts {
                            if echo {
                                eprint!("{text}");
                                let mut line = String::new();
                                std::io::stdin().read_line(&mut line).ok()?;
                                answers.push(line.trim_end_matches(['\r', '\n']).to_owned());
                            } else {
                                answers.push(rpassword::prompt_password(text).ok()?);
                            }
                        }
                        Some(answers)
                    })
                    .await
                    .ok()
                    .flatten()
                })
            }));
        }
        return Ok(Target::Sftp(cfg, path));
    }

    if matches!(url.scheme(), "dav" | "davs" | "webdav" | "webdavs") {
        let host = url.host_str().context("URL has no host")?;
        let scheme = if url.scheme().ends_with('s') { "https" } else { "http" };
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        // The server root is the base; the whole URL path is the remote path.
        let mut cfg = WebdavConfig::new(&format!("{scheme}://{host}{port}/"))?;
        cfg.user = decode(url.username());
        cfg.password = match url_password {
            Some(p) => p,
            None if !cfg.user.is_empty() => ask_password(&cfg.user, host)?,
            None => String::new(),
        };
        cfg.accept_invalid_certs = opts.insecure;
        cfg.timeout = Duration::from_secs(opts.timeout);
        return Ok(Target::Webdav(cfg, path));
    }

    let (security, default_port) = match url.scheme() {
        "ftp" if opts.plain => (Security::Plain, 21),
        "ftp" => (Security::TryTls, 21),
        "ftpes" => (Security::ExplicitTls, 21),
        "ftps" => (Security::ImplicitTls, 990),
        other => bail!("unsupported scheme {other}:// (use ftp, ftpes, ftps, sftp, dav, davs or s3)"),
    };
    let host = url.host_str().context("URL has no host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();

    let mut cfg = FtpConfig::new(host);
    cfg.port = url.port().unwrap_or(default_port);
    cfg.security = security;
    cfg.accept_invalid_certs = opts.insecure;
    cfg.verbose = opts.verbose;
    cfg.timeout = Duration::from_secs(opts.timeout);
    cfg.user = decode(url.username());
    cfg.password = match url_password {
        Some(p) => p,
        None if !cfg.user.is_empty() && cfg.user != "anonymous" => ask_password(&cfg.user, &cfg.host)?,
        None => String::new(),
    };
    Ok(Target::Ftp(cfg, path))
}

fn s3_config(opts: &Opts) -> S3Config {
    S3Config {
        endpoint: opts.endpoint.clone(),
        region: opts.region.clone(),
        profile: opts.profile.clone(),
        access_key: opts.access_key.clone(),
        secret_key: opts.secret_key.clone(),
        session_token: None,
        path_style: opts.path_style,
        upload: UploadOptions {
            storage_class: opts.storage_class.clone(),
            sse: opts.sse.clone(),
            sse_kms_key_id: opts.sse_kms_key_id.clone(),
            part_size: opts.part_size * 1024 * 1024,
            multipart_threshold: opts.part_size * 1024 * 1024,
            concurrency: opts.concurrency,
        },
    }
}

// ---------------------------------------------------------------------------
// Progress display
// ---------------------------------------------------------------------------

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

struct Progress {
    name: String,
    total: Option<u64>,
    start: Instant,
    last: Instant,
    tty: bool,
}

impl Progress {
    fn new(name: &str, total: Option<u64>) -> Self {
        let now = Instant::now();
        Self { name: name.to_owned(), total, start: now, last: now, tty: std::io::stderr().is_terminal() }
    }

    fn update(&mut self, done: u64) {
        if !self.tty || self.last.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.last = Instant::now();
        let rate = done as f64 / self.start.elapsed().as_secs_f64().max(0.001);
        let total = self.total.map(|t| format!(" / {}", human(t))).unwrap_or_default();
        eprint!("\r\x1b[K{}  {}{}  {}/s", self.name, human(done), total, human(rate as u64));
        let _ = std::io::stderr().flush();
    }

    fn finish(&self, done: u64) {
        let secs = self.start.elapsed().as_secs_f64();
        let rate = done as f64 / secs.max(0.001);
        let clear = if self.tty { "\r\x1b[K" } else { "" };
        eprintln!("{clear}{}  {} in {:.1}s ({}/s)", self.name, human(done), secs, human(rate as u64));
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Print per-file progress for tree transfers.
fn tree_reporter() -> impl FnMut(Event) {
    let mut current: Option<Progress> = None;
    move |ev| match ev {
        Event::FileStart { remote, size, .. } => current = Some(Progress::new(basename(&remote), size)),
        Event::Progress { done } => {
            if let Some(p) = current.as_mut() {
                p.update(done);
            }
        }
        Event::FileDone { bytes } => {
            if let Some(p) = current.take() {
                p.finish(bytes);
            }
        }
        Event::Skipped { path, reason } => eprintln!("skipping {path} ({reason})"),
        Event::Failed { path, error } => {
            if current.take().is_some() && std::io::stderr().is_terminal() {
                eprint!("\r\x1b[K");
            }
            eprintln!("FAILED {path}: {error}");
        }
    }
}

fn transfer_opts(opts: &Opts) -> Result<TransferOpts> {
    let action: ExistsAction = if opts.resume { ExistsAction::Resume } else { opts.if_exists.parse().map_err(anyhow::Error::msg)? };
    if action == ExistsAction::Ask {
        bail!("--if-exists ask is only available in the desktop app");
    }
    let t = TransferOpts { download_exists: action, upload_exists: action, preserve_time: opts.preserve_time, ..Default::default() };
    t.limiter.set_kib(opts.limit_rate, opts.limit_rate);
    Ok(t)
}

fn report(name: &str, p: &Progress, outcome: Outcome) {
    match outcome {
        Outcome::Done { bytes, resumed_from } => {
            if resumed_from > 0 {
                eprintln!("{name}: resumed at {}", human(resumed_from));
            }
            p.finish(bytes);
        }
        Outcome::Skipped => eprintln!("{name}: skipped (target exists)"),
    }
}

async fn download_one(b: &mut Backend, remote: &str, local: &Path, t: &TransferOpts) -> Result<()> {
    let mut p = Progress::new(basename(remote), None);
    let outcome = b.download_file(remote, local, None, t, &mut always(Decision::Overwrite), &mut |n| p.update(n)).await?;
    report(basename(remote), &p, outcome);
    Ok(())
}

async fn upload_one(b: &mut Backend, local: &Path, remote: &str, t: &TransferOpts) -> Result<()> {
    let size = tokio::fs::metadata(local).await?.len();
    let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut p = Progress::new(&name, Some(size));
    let outcome = b.upload_file(local, remote, None, t, &mut always(Decision::Overwrite), &mut |n| p.update(n)).await?;
    report(&name, &p, outcome);
    Ok(())
}

fn print_listing(entries: &[DirEntry], long: bool) {
    for e in entries {
        let suffix = if e.is_dir() { "/" } else { "" };
        let target = e.target.as_deref().map(|t| format!(" -> {t}")).unwrap_or_default();
        if long {
            let size = if e.size >= 0 { e.size.to_string() } else { String::new() };
            let time = if e.has_date() { e.time.to_string() } else { String::new() };
            println!(
                "{:<11} {:<17} {:>13}  {:<16}  {}{}{}",
                e.permissions, e.owner_group, size, time, e.name, suffix, target
            );
        } else {
            println!("{}{}", e.name, suffix);
        }
    }
}

/// Resolve the `to` argument of `mv` against the source path.
fn resolve_to(from: &str, to: &str, s3: bool, opts: &Opts) -> Result<String> {
    if to.contains("://") {
        return Ok(match parse_target(to, opts)? {
            Target::S3(p) => p,
            Target::Ftp(_, p) | Target::Sftp(_, p) | Target::Webdav(_, p) => p,
        });
    }
    if let Some(abs) = to.strip_prefix('/') {
        return Ok(if s3 { abs.to_owned() } else { to.to_owned() });
    }
    Ok(join(parent(from), to))
}

async fn exec(r: &mut Backend, cmd: &Cmd, path: &str, opts: &Opts) -> Result<()> {
    let s3 = r.implicit_dirs();
    match cmd {
        Cmd::Ls { l, .. } => {
            print_listing(&r.list(path).await?, *l);
        }
        Cmd::Get { local, recursive, .. } => {
            let name = safe_name(basename(path)).unwrap_or("download");
            let dest = match local {
                Some(l) if l.is_dir() => l.join(name),
                Some(l) => l.clone(),
                None => PathBuf::from(name),
            };
            let t = transfer_opts(opts)?;
            if *recursive {
                r.download_tree(path, &dest, &t, &mut always(Decision::Overwrite), &mut tree_reporter()).await?;
            } else {
                download_one(r, path, &dest, &t).await?;
            }
        }
        Cmd::Put { local, recursive, .. } => {
            let name = local
                .file_name()
                .context("local path has no file name")?
                .to_string_lossy()
                .into_owned();
            let dest = if path.ends_with('/') || r.is_dir(path).await? { join(path, &name) } else { path.to_owned() };
            if *recursive {
                if !local.is_dir() {
                    bail!("{} is not a directory", local.display());
                }
                r.upload_tree(local, &dest, &transfer_opts(opts)?, &mut always(Decision::Overwrite), &mut tree_reporter()).await?;
            } else {
                if local.is_dir() {
                    bail!("{} is a directory (use -r)", local.display());
                }
                upload_one(r, local, &dest, &transfer_opts(opts)?).await?;
            }
        }
        Cmd::Mkdir { .. } => r.mkdir(path).await?,
        Cmd::Rm { recursive, .. } => {
            if *recursive {
                r.rmdir(path, true).await?;
            } else {
                r.delete(path).await?;
            }
        }
        Cmd::Rmdir { .. } => r.rmdir(path, false).await?,
        Cmd::Mv { to, force, .. } => {
            let to = resolve_to(path, to, s3, opts)?;
            r.rename(path, &to, *force).await?;
        }
        Cmd::Chmod { .. } | Cmd::Url { .. } => unreachable!("handled per backend"),
    }
    Ok(())
}

/// Run one command on a connected backend and close it.
async fn run(mut remote: Backend, cmd: &Cmd, path: &str, opts: &Opts) -> Result<()> {
    let res = match cmd {
        Cmd::Chmod { mode, .. } => remote.chmod(path, mode).await,
        cmd => exec(&mut remote, cmd, path, opts).await,
    };
    remote.close().await;
    res
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = &cli.opts;
    match parse_target(cli.cmd.url(), opts)? {
        Target::Ftp(cfg, path) => {
            if matches!(cli.cmd, Cmd::Url { .. }) {
                bail!("pre-signed URLs are only available for s3:// targets");
            }
            let client = FtpClient::connect(&cfg)
                .await
                .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
            if !client.is_secure() && cfg.security == Security::TryTls {
                eprintln!("warning: connection is not encrypted");
            }
            run(Backend::Ftp(client), &cli.cmd, &path, opts).await
        }
        Target::Sftp(mut cfg, path) => {
            if matches!(cli.cmd, Cmd::Url { .. }) {
                bail!("pre-signed URLs are only available for s3:// targets");
            }
            let client = match SftpClient::connect(&cfg).await {
                Ok(c) => c,
                // Agent and default keys did not work: ask for a password.
                Err(e)
                    if cfg.password.is_empty()
                        && cfg.key_file.is_none()
                        && e.to_string().contains("authentication failed")
                        && std::io::stdin().is_terminal() =>
                {
                    let user = if cfg.user.is_empty() { std::env::var("USER").unwrap_or_default() } else { cfg.user.clone() };
                    cfg.password = ask_password(&user, &cfg.host)?;
                    SftpClient::connect(&cfg).await?
                }
                // Encrypted key and no passphrase given: ask for it.
                Err(e) if e.to_string().starts_with("KEY_PASSPHRASE_NEEDED") && std::io::stdin().is_terminal() => {
                    let file = cfg.key_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                    cfg.key_passphrase = Some(rpassword::prompt_password(format!("Passphrase for key {file}: "))?);
                    SftpClient::connect(&cfg).await?
                }
                Err(e) => {
                    if e.to_string().starts_with("UNKNOWN_HOST_KEY") {
                        bail!("{e}\nVerify the fingerprint with the server administrator, then re-run with --accept-new-host-key.");
                    }
                    return Err(e);
                }
            };
            run(Backend::Sftp(client), &cli.cmd, &path, opts).await
        }
        Target::Webdav(cfg, path) => {
            if matches!(cli.cmd, Cmd::Url { .. }) {
                bail!("pre-signed URLs are only available for s3:// targets");
            }
            run(Backend::Webdav(WebdavClient::new(&cfg)?), &cli.cmd, &path, opts).await
        }
        Target::S3(path) => {
            let client = S3Client::connect(&s3_config(opts)).await?;
            match &cli.cmd {
                Cmd::Url { expires, .. } => {
                    println!("{}", client.presign(&path, Duration::from_secs(*expires)).await?);
                    Ok(())
                }
                cmd => run(Backend::S3(client), cmd, &path, opts).await,
            }
        }
    }
}

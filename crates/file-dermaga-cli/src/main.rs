//! `file-dermaga`: command line FTP / FTPS / S3 client.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use file_dermaga_core::DirEntry;
use file_dermaga_ftp::{FtpClient, FtpConfig, Security};
use file_dermaga_transfer::{basename, join, parent, safe_name, Backend, Event};
use file_dermaga_s3::{S3Client, S3Config, UploadOptions};
use percent_encoding::percent_decode_str;

#[derive(Parser)]
#[command(name = "file-dermaga", version, about = "FTP, FTPS and S3 client")]
#[command(after_help = "\
URLs:
  ftp://[user[:pass]@]host[:port]/path    FTP, upgrades to TLS when offered
  ftpes://user@host/path                  FTP with required explicit TLS
  ftps://user@host/path                   FTP with implicit TLS (port 990)
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
    /// Accept invalid or self-signed TLS certificates
    #[arg(short = 'k', long, global = true)]
    insecure: bool,
    /// Show the FTP command/response log
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Network timeout in seconds
    #[arg(long, default_value_t = 20, global = true)]
    timeout: u64,

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
    Mv { url: String, to: String },
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
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn parse_target(s: &str, opts: &Opts) -> Result<Target> {
    if let Some(rest) = s.strip_prefix("s3://") {
        return Ok(Target::S3(rest.to_owned()));
    }
    let url = url::Url::parse(s).with_context(|| format!("invalid URL: {s}"))?;
    let (security, default_port) = match url.scheme() {
        "ftp" if opts.plain => (Security::Plain, 21),
        "ftp" => (Security::TryTls, 21),
        "ftpes" => (Security::ExplicitTls, 21),
        "ftps" => (Security::ImplicitTls, 990),
        other => bail!("unsupported scheme {other}:// (use ftp, ftpes, ftps or s3)"),
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
    cfg.password = match (url.password(), &opts.password) {
        (Some(p), _) => decode(p),
        (None, Some(p)) => p.clone(),
        (None, None) if !cfg.user.is_empty() && cfg.user != "anonymous" => {
            rpassword::prompt_password(format!("Password for {}@{}: ", cfg.user, cfg.host))?
        }
        _ => String::new(),
    };
    let path = decode(url.path());
    Ok(Target::Ftp(cfg, if path.is_empty() { "/".into() } else { path }))
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

async fn download_one(b: &mut Backend, remote: &str, local: &Path) -> Result<()> {
    let mut p = Progress::new(basename(remote), None);
    let n = b.download(remote, local, &mut |n| p.update(n)).await?;
    p.finish(n);
    Ok(())
}

async fn upload_one(b: &mut Backend, local: &Path, remote: &str) -> Result<()> {
    let size = tokio::fs::metadata(local).await?.len();
    let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut p = Progress::new(&name, Some(size));
    let n = b.upload(local, remote, &mut |n| p.update(n)).await?;
    p.finish(n);
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
            Target::Ftp(_, p) => p,
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
            if *recursive {
                r.download_tree(path, &dest, &mut tree_reporter()).await?;
            } else {
                download_one(r, path, &dest).await?;
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
                r.upload_tree(local, &dest, &mut tree_reporter()).await?;
            } else {
                if local.is_dir() {
                    bail!("{} is a directory (use -r)", local.display());
                }
                upload_one(r, local, &dest).await?;
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
        Cmd::Mv { to, .. } => {
            let to = resolve_to(path, to, s3, opts)?;
            r.rename(path, &to).await?;
        }
        Cmd::Chmod { .. } | Cmd::Url { .. } => unreachable!("handled per backend"),
    }
    Ok(())
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
            let mut remote = Backend::Ftp(client);
            let res = match (&cli.cmd, &mut remote) {
                (Cmd::Chmod { mode, .. }, Backend::Ftp(c)) => c.chmod(&path, mode).await.map_err(Into::into),
                (cmd, remote) => exec(remote, cmd, &path, opts).await,
            };
            remote.close().await;
            res
        }
        Target::S3(path) => {
            let client = S3Client::connect(&s3_config(opts)).await?;
            match &cli.cmd {
                Cmd::Url { expires, .. } => {
                    println!("{}", client.presign(&path, Duration::from_secs(*expires)).await?);
                    Ok(())
                }
                Cmd::Chmod { .. } => bail!("chmod is not supported for S3"),
                cmd => exec(&mut Backend::S3(client), cmd, &path, opts).await,
            }
        }
    }
}

//! Backend-neutral file operations shared by the CLI and the GUI.
//!
//! Paths are strings: `/dir/file` for FTP, SFTP and WebDAV (WebDAV paths
//! are relative to the server's base URL), `bucket/key` for S3 (empty =
//! bucket list).

mod policy;
mod throttle;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use file_dermaga_core::{Accuracy, DateTime, DirEntry};
use tokio::io::{AsyncSeekExt, AsyncWrite};

pub use policy::{decide, BoxFuture, Conflict, Decision, Direction, ExistsAction, FileInfo, Resolver};
pub use throttle::{Bucket, Limiter, ThrottledReader, ThrottledWriter};
use file_dermaga_ftp::FtpClient;
use file_dermaga_s3::S3Client;
use file_dermaga_sftp::SftpClient;
use file_dermaga_webdav::WebdavClient;
use tokio::io::{AsyncWriteExt, BufWriter};

pub enum Backend {
    Ftp(FtpClient),
    S3(S3Client),
    Sftp(SftpClient),
    Webdav(WebdavClient),
}

/// Options shared by all transfers of a queue.
#[derive(Clone, Debug, Default)]
pub struct TransferOpts {
    /// What to do with existing local files on download.
    pub download_exists: ExistsAction,
    /// What to do with existing remote files on upload.
    pub upload_exists: ExistsAction,
    /// Keep the source's modification time on the target.
    pub preserve_time: bool,
    pub limiter: Limiter,
}

impl TransferOpts {
    /// Overwrite without asking (scripts, tests).
    pub fn overwrite() -> Self {
        Self { download_exists: ExistsAction::Overwrite, upload_exists: ExistsAction::Overwrite, ..Default::default() }
    }
}

/// Result of transferring one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `bytes` were moved now; the first `resumed_from` bytes already existed.
    Done { bytes: u64, resumed_from: u64 },
    /// Not transferred because of the exists policy (or already complete).
    Skipped,
}

/// A resolver that always gives the same answer (for non-interactive use).
pub fn always(d: Decision) -> impl FnMut(Conflict) -> BoxFuture<Decision> + Send {
    move |_| {
        let d = d.clone();
        Box::pin(async move { d })
    }
}

fn info_of(e: &DirEntry) -> FileInfo {
    FileInfo { size: u64::try_from(e.size).ok(), mtime: (!e.time.is_empty()).then_some(e.time) }
}

fn local_info(meta: &std::fs::Metadata) -> FileInfo {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| DateTime::from_unix_secs(d.as_secs() as i64, Accuracy::Seconds));
    FileInfo { size: Some(meta.len()), mtime }
}

fn set_local_mtime(path: &Path, time: &DateTime) -> std::io::Result<()> {
    let Some(ms) = time.unix_millis() else { return Ok(()) };
    let t = if ms >= 0 { UNIX_EPOCH + Duration::from_millis(ms as u64) } else { UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs()) };
    std::fs::File::options().write(true).open(path)?.set_modified(t)
}

async fn resolve_conflict(opts: &TransferOpts, c: Conflict, resolve: Resolver<'_>) -> Decision {
    let action = match c.direction {
        Direction::Download => opts.download_exists,
        Direction::Upload => opts.upload_exists,
    };
    match policy::decide(action, &c.source, &c.target) {
        Some(d) => d,
        None => resolve(c).await,
    }
}

/// Progress of a (possibly recursive) transfer.
#[derive(Clone, Debug)]
pub enum Event {
    FileStart { remote: String, local: PathBuf, size: Option<u64> },
    Progress { done: u64 },
    FileDone { bytes: u64 },
    Skipped { path: String, reason: String },
    /// One file failed; the tree transfer continues with the next file.
    Failed { path: String, error: String },
}

/// One file of a directory tree (see `plan_download` / `plan_upload`).
#[derive(Clone, Debug)]
pub struct TreeFile {
    /// Path below the tree's root, with `/` separators ("sub/a.txt").
    pub rel: String,
    pub info: FileInfo,
}

pub fn join(base: &str, name: &str) -> String {
    file_dermaga_ftp::join(base, name)
}

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

pub fn parent(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "",
    }
}

/// Names from a server end up as local file names: refuse anything that
/// could escape the target directory.
pub fn safe_name(name: &str) -> Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        bail!("refusing unsafe file name from server: {name:?}");
    }
    Ok(name)
}

impl Backend {
    /// Whether directories exist implicitly (S3 prefixes).
    pub fn implicit_dirs(&self) -> bool {
        matches!(self, Backend::S3(_))
    }

    pub async fn list(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let mut entries = match self {
            Backend::Ftp(c) => c.list(path).await?.entries,
            Backend::S3(c) => c.list(path).await?,
            Backend::Sftp(c) => c.list(path).await?,
            Backend::Webdav(c) => c.list(path).await?,
        };
        entries.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    pub async fn is_dir(&mut self, path: &str) -> Result<bool> {
        Ok(match self {
            Backend::Ftp(c) => c.cwd(path).await.is_ok(),
            Backend::S3(_) => {
                let (_, key) = file_dermaga_s3::split(path);
                key.is_empty() || key.ends_with('/')
            }
            Backend::Sftp(c) => c.is_dir(path).await?,
            Backend::Webdav(c) => c.is_dir(path).await?,
        })
    }

    /// Download one file. A partial local file is removed on failure.
    pub async fn download(&mut self, path: &str, local: &Path, progress: &mut (dyn FnMut(u64) + Send)) -> Result<u64> {
        let res = async {
            let mut out = BufWriter::new(tokio::fs::File::create(local).await?);
            let n = match self {
                Backend::Ftp(c) => c.retrieve(path, &mut out, &mut *progress).await?,
                Backend::S3(c) => c.download(path, &mut out, &mut *progress).await?,
                Backend::Sftp(c) => c.download(path, &mut out, &mut *progress).await?,
                Backend::Webdav(c) => c.download(path, &mut out, &mut *progress).await?,
            };
            out.shutdown().await?;
            Ok::<_, anyhow::Error>(n)
        }
        .await;
        if res.is_err() {
            let _ = tokio::fs::remove_file(local).await;
        }
        res.with_context(|| format!("downloading {path}"))
    }

    pub async fn upload(&mut self, local: &Path, path: &str, progress: &mut (dyn FnMut(u64) + Send)) -> Result<u64> {
        let res = match self {
            Backend::Ftp(c) => {
                let mut input = tokio::fs::File::open(local).await?;
                c.store(path, &mut input, &mut *progress).await.map_err(Into::into)
            }
            Backend::S3(c) => c.upload(local, path, &mut *progress).await,
            Backend::Sftp(c) => {
                let mut input = tokio::fs::File::open(local).await?;
                c.upload(path, &mut input, &mut *progress).await
            }
            Backend::Webdav(c) => {
                let input = tokio::fs::File::open(local).await?;
                let size = input.metadata().await?.len();
                c.upload(path, input, size, &mut *progress).await
            }
        };
        res.with_context(|| format!("uploading {}", local.display()))
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<()> {
        match self {
            Backend::Ftp(c) => Ok(c.mkdir(path).await?),
            Backend::S3(c) => c.mkdir(path).await,
            Backend::Sftp(c) => c.mkdir(path).await,
            Backend::Webdav(c) => c.mkdir(path).await,
        }
    }

    /// Create a directory and any missing parents (`mkdir -p`). Errors for
    /// components that already exist are ignored; the final directory must
    /// exist afterwards.
    pub async fn mkdir_all(&mut self, path: &str) -> Result<()> {
        if self.implicit_dirs() {
            return Ok(());
        }
        let absolute = path.starts_with('/');
        let mut cur = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if absolute || !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(part);
            let _ = self.mkdir(&cur).await;
        }
        if !self.is_dir(path).await? {
            bail!("could not create directory {path}");
        }
        Ok(())
    }

    pub async fn delete(&mut self, path: &str) -> Result<()> {
        match self {
            Backend::Ftp(c) => Ok(c.delete(path).await?),
            Backend::S3(c) => c.delete(path).await,
            Backend::Sftp(c) => c.delete(path).await,
            Backend::Webdav(c) => c.delete(path).await,
        }
    }

    pub async fn rmdir(&mut self, path: &str, recursive: bool) -> Result<()> {
        match self {
            Backend::Ftp(c) if recursive => Ok(c.remove_recursive(path).await?),
            Backend::Ftp(c) => Ok(c.rmdir(path).await?),
            Backend::S3(c) => c.rmdir(path, recursive).await,
            Backend::Sftp(c) if recursive => c.remove_recursive(path).await,
            Backend::Sftp(c) => c.rmdir(path).await,
            Backend::Webdav(c) => c.rmdir(path, recursive).await,
        }
    }

    /// Rename/move. `overwrite` replaces an existing target where the
    /// protocol can (SFTP posix-rename, WebDAV `Overwrite: T`); FTP servers
    /// and S3 replace targets on their own.
    pub async fn rename(&mut self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        match self {
            Backend::Ftp(c) => Ok(c.rename(from, to).await?),
            Backend::S3(c) => c.rename(from, to).await,
            Backend::Sftp(c) => c.rename(from, to, overwrite).await,
            Backend::Webdav(c) => c.rename(from, to, overwrite).await,
        }
    }

    /// Size/time of a remote path; `None` if it does not exist.
    pub async fn stat(&mut self, path: &str) -> Result<Option<DirEntry>> {
        match self {
            Backend::Ftp(c) => {
                let size = c.size(path).await?;
                let mtime = c.mdtm(path).await?;
                Ok((size.is_some() || mtime.is_some()).then(|| DirEntry {
                    name: basename(path).to_owned(),
                    size: size.map(|s| s as i64).unwrap_or(-1),
                    time: mtime.unwrap_or_default(),
                    ..Default::default()
                }))
            }
            Backend::S3(c) => c.stat(path).await,
            Backend::Sftp(c) => c.stat(path).await,
            Backend::Webdav(c) => c.stat(path).await,
        }
    }

    fn can_resume_download(&self) -> bool {
        match self {
            Backend::Ftp(c) => c.features().rest_stream,
            _ => true,
        }
    }

    fn can_resume_upload(&self) -> bool {
        matches!(self, Backend::Ftp(_) | Backend::Sftp(_))
    }

    async fn fetch<W: AsyncWrite + Unpin + Send>(
        &mut self,
        remote: &str,
        out: &mut W,
        offset: u64,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<u64> {
        Ok(match self {
            Backend::Ftp(c) => c.retrieve_from(remote, out, offset, &mut *progress).await?,
            Backend::Sftp(c) => c.download_from(remote, out, offset, &mut *progress).await?,
            Backend::S3(c) => c.download_from(remote, out, offset, &mut *progress).await?,
            Backend::Webdav(c) => c.download_from(remote, out, offset, &mut *progress).await?.0,
        })
    }

    /// Download one file, applying the exists policy, resume, the speed
    /// limit and (optionally) the remote modification time. `known` is the
    /// remote size/time if a listing already provided it.
    pub async fn download_file(
        &mut self,
        remote: &str,
        local: &Path,
        known: Option<FileInfo>,
        opts: &TransferOpts,
        resolve: Resolver<'_>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<Outcome> {
        let mut local = local.to_path_buf();
        let mut remote_info = known;
        let mut offset = 0u64;
        for _ in 0..5 {
            let Ok(meta) = tokio::fs::metadata(&local).await else { break };
            if !meta.is_file() {
                bail!("{} exists and is not a file", local.display());
            }
            let ri = match remote_info {
                Some(i) => i,
                None => {
                    let i = self.stat(remote).await.ok().flatten().map(|e| info_of(&e)).unwrap_or_default();
                    remote_info = Some(i);
                    i
                }
            };
            let c = Conflict {
                direction: Direction::Download,
                source_path: remote.to_owned(),
                target_path: local.display().to_string(),
                source: ri,
                target: local_info(&meta),
            };
            match resolve_conflict(opts, c, &mut *resolve).await {
                Decision::Skip => return Ok(Outcome::Skipped),
                Decision::Overwrite => break,
                Decision::Resume => {
                    if ri.size.is_some_and(|total| meta.len() >= total) {
                        return Ok(Outcome::Skipped); // already complete
                    }
                    if self.can_resume_download() {
                        offset = meta.len();
                    }
                    break;
                }
                Decision::Rename(name) => local = local.with_file_name(safe_name(&name)?),
            }
        }

        let bytes = loop {
            let file = if offset > 0 {
                tokio::fs::OpenOptions::new().append(true).open(&local).await?
            } else {
                tokio::fs::File::create(&local).await?
            };
            let mut out = ThrottledWriter::new(BufWriter::new(file), opts.limiter.down.clone());
            let base = offset;
            match self.fetch(remote, &mut out, base, &mut |n| progress(base + n)).await {
                // The server ignored the range: start again from scratch.
                Err(e) if base > 0 && format!("{e:#}").contains("RANGE_IGNORED") => offset = 0,
                Err(e) => return Err(e.context(format!("downloading {remote}"))),
                Ok(n) => {
                    out.shutdown().await?;
                    break n;
                }
            }
        };

        if opts.preserve_time {
            let mtime = match remote_info.and_then(|i| i.mtime) {
                Some(t) => Some(t),
                None => self.stat(remote).await.ok().flatten().filter(|e| e.has_date()).map(|e| e.time),
            };
            if let Some(t) = mtime {
                let _ = set_local_mtime(&local, &t);
            }
        }
        Ok(Outcome::Done { bytes, resumed_from: offset })
    }

    /// Upload one file with the exists policy, resume (FTP, SFTP), the speed
    /// limit and (optionally) the local modification time. `known` is
    /// `Some(None)` if the target is known not to exist, `Some(Some(info))`
    /// if a listing showed it, and `None` to look it up.
    pub async fn upload_file(
        &mut self,
        local: &Path,
        remote: &str,
        known: Option<Option<FileInfo>>,
        opts: &TransferOpts,
        resolve: Resolver<'_>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<Outcome> {
        let meta = tokio::fs::metadata(local).await.with_context(|| format!("reading {}", local.display()))?;
        let li = local_info(&meta);
        let mut remote = remote.to_owned();
        let mut known = known;
        let mut offset = 0u64;
        for _ in 0..5 {
            let existing = match known.take() {
                Some(k) => k,
                None => self.stat(&remote).await.ok().flatten().filter(|e| !e.is_dir()).map(|e| info_of(&e)),
            };
            let Some(ri) = existing else { break };
            let c = Conflict {
                direction: Direction::Upload,
                source_path: local.display().to_string(),
                target_path: remote.clone(),
                source: li,
                target: ri,
            };
            match resolve_conflict(opts, c, &mut *resolve).await {
                Decision::Skip => return Ok(Outcome::Skipped),
                Decision::Overwrite => break,
                Decision::Resume => {
                    match ri.size {
                        Some(have) if have >= meta.len() => return Ok(Outcome::Skipped),
                        Some(have) if self.can_resume_upload() => offset = have,
                        _ => {} // cannot resume: overwrite
                    }
                    break;
                }
                Decision::Rename(name) => {
                    remote = join(parent(&remote), safe_name(&name)?);
                }
            }
        }

        let mut file = tokio::fs::File::open(local).await?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset)).await?;
        }
        let up = opts.limiter.up.clone();
        let base = offset;
        let mut prog = |n: u64| progress(base + n);
        let res = match self {
            Backend::Ftp(c) => {
                let mut r = ThrottledReader::new(file, up);
                c.store_mode(&remote, &mut r, offset > 0, &mut prog).await.map_err(Into::into)
            }
            Backend::Sftp(c) => {
                let mut r = ThrottledReader::new(file, up);
                c.upload_from(&remote, &mut r, offset, &mut prog).await
            }
            Backend::S3(c) => c.upload(local, &remote, &mut prog).await,
            Backend::Webdav(c) => {
                let mtime = if opts.preserve_time { li.mtime.and_then(|t| t.unix_millis()).map(|ms| ms / 1000) } else { None };
                c.upload_with_mtime(&remote, ThrottledReader::new(file, up), meta.len(), mtime, &mut prog).await
            }
        };
        let bytes = res.with_context(|| format!("uploading {}", local.display()))?;

        if opts.preserve_time {
            if let Some(t) = li.mtime {
                // Best effort: a refused timestamp does not fail the upload.
                let _ = match self {
                    Backend::Ftp(c) => c.set_mtime(&remote, &t).await.map(drop).map_err(anyhow::Error::from),
                    Backend::Sftp(c) => c.set_mtime(&remote, &t).await,
                    Backend::S3(_) | Backend::Webdav(_) => Ok(()),
                };
            }
        }
        Ok(Outcome::Done { bytes, resumed_from: offset })
    }

    /// Change permissions; `mode` is octal like "644" (FTP and SFTP only).
    pub async fn chmod(&mut self, path: &str, mode: &str) -> Result<()> {
        match self {
            Backend::Ftp(c) => Ok(c.chmod(path, mode).await?),
            Backend::Sftp(c) => c.chmod(path, mode).await,
            Backend::S3(_) => bail!("S3 has no file permissions"),
            Backend::Webdav(_) => bail!("WebDAV has no file permissions"),
        }
    }

    pub async fn close(self) {
        match self {
            Backend::Ftp(c) => {
                let _ = c.quit().await;
            }
            Backend::Sftp(c) => c.close().await,
            Backend::S3(_) | Backend::Webdav(_) => {}
        }
    }

    /// Walk the remote tree `remote` for a download that is queued file by file:
    /// creates the local directories (empty ones too) and returns every file,
    /// sorted by its path relative to `remote`.
    pub async fn plan_download(&mut self, remote: &str, local: &Path, ev: &mut (dyn FnMut(Event) + Send)) -> Result<Vec<TreeFile>> {
        let mut files = Vec::new();
        let mut stack = vec![(remote.to_owned(), local.to_path_buf(), String::new())];
        while let Some((rdir, ldir, rel)) = stack.pop() {
            tokio::fs::create_dir_all(&ldir).await?;
            for e in self.list(&rdir).await? {
                let name = safe_name(&e.name)?;
                let rp = join(&rdir, name);
                let r = if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
                if e.is_dir() {
                    if e.is_link() {
                        ev(Event::Skipped { path: rp, reason: "symbolic link".into() });
                    } else {
                        stack.push((rp, ldir.join(name), r));
                    }
                } else {
                    files.push(TreeFile { rel: r, info: info_of(&e) });
                }
            }
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(files)
    }

    /// Walk the local tree `local` for an upload that is queued file by file:
    /// creates the remote directories (empty ones too) and returns every file,
    /// sorted by its path relative to `local`.
    pub async fn plan_upload(&mut self, local: &Path, remote: &str, ev: &mut (dyn FnMut(Event) + Send)) -> Result<Vec<TreeFile>> {
        self.mkdir_all(remote).await?;
        let mut files = Vec::new();
        let mut stack = vec![(local.to_path_buf(), remote.to_owned(), String::new())];
        while let Some((ldir, rdir, rel)) = stack.pop() {
            if !rel.is_empty() && !self.implicit_dirs() {
                // Parents exist by now; fails harmlessly if it already exists.
                let _ = self.mkdir(&rdir).await;
            }
            let mut rd = tokio::fs::read_dir(&ldir).await?;
            while let Some(de) = rd.next_entry().await? {
                let name = de.file_name().to_string_lossy().into_owned();
                let path = de.path();
                let meta = tokio::fs::metadata(&path).await?;
                let is_link = tokio::fs::symlink_metadata(&path).await?.file_type().is_symlink();
                let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                if meta.is_dir() {
                    if is_link {
                        ev(Event::Skipped { path: path.display().to_string(), reason: "symbolic link".into() });
                    } else {
                        stack.push((path, join(&rdir, &name), r));
                    }
                } else if meta.is_file() {
                    files.push(TreeFile { rel: r, info: local_info(&meta) });
                }
            }
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(files)
    }

    /// Download a directory tree into `local`.
    pub async fn download_tree(
        &mut self,
        remote: &str,
        local: &Path,
        opts: &TransferOpts,
        resolve: Resolver<'_>,
        ev: &mut (dyn FnMut(Event) + Send),
    ) -> Result<()> {
        let mut failed = 0usize;
        let mut stack = vec![(remote.to_owned(), local.to_path_buf())];
        while let Some((rdir, ldir)) = stack.pop() {
            tokio::fs::create_dir_all(&ldir).await?;
            for e in self.list(&rdir).await? {
                let name = safe_name(&e.name)?;
                let (rp, lp) = (join(&rdir, name), ldir.join(name));
                if e.is_dir() {
                    if e.is_link() {
                        ev(Event::Skipped { path: rp, reason: "symbolic link".into() });
                    } else {
                        stack.push((rp, lp));
                    }
                    continue;
                }
                ev(Event::FileStart { remote: rp.clone(), local: lp.clone(), size: u64::try_from(e.size).ok() });
                let r = self.download_file(&rp, &lp, Some(info_of(&e)), opts, &mut *resolve, &mut |done| ev(Event::Progress { done })).await;
                match r {
                    Ok(Outcome::Done { bytes, .. }) => ev(Event::FileDone { bytes }),
                    Ok(Outcome::Skipped) => ev(Event::Skipped { path: rp, reason: "target exists".into() }),
                    Err(e) => {
                        failed += 1;
                        ev(Event::Failed { path: rp, error: format!("{e:#}") });
                    }
                }
            }
        }
        if failed > 0 {
            bail!("{failed} file(s) failed to download");
        }
        Ok(())
    }

    /// Upload a local directory tree to `remote`.
    pub async fn upload_tree(
        &mut self,
        local: &Path,
        remote: &str,
        opts: &TransferOpts,
        resolve: Resolver<'_>,
        ev: &mut (dyn FnMut(Event) + Send),
    ) -> Result<()> {
        self.mkdir_all(remote).await?;
        let mut failed = 0usize;
        let mut stack = vec![(local.to_path_buf(), remote.to_owned())];
        let mut first = true;
        while let Some((ldir, rdir)) = stack.pop() {
            if !first && !self.implicit_dirs() {
                // Parents exist by now; fails harmlessly if it already exists.
                let _ = self.mkdir(&rdir).await;
            }
            first = false;
            // One listing per directory tells which targets already exist.
            let existing: HashMap<String, DirEntry> =
                self.list(&rdir).await.unwrap_or_default().into_iter().map(|e| (e.name.clone(), e)).collect();
            let mut rd = tokio::fs::read_dir(&ldir).await?;
            while let Some(de) = rd.next_entry().await? {
                let name = de.file_name().to_string_lossy().into_owned();
                let path = de.path();
                let meta = tokio::fs::metadata(&path).await?;
                let is_link = tokio::fs::symlink_metadata(&path).await?.file_type().is_symlink();
                let rp = join(&rdir, &name);
                if meta.is_dir() {
                    if is_link {
                        ev(Event::Skipped { path: path.display().to_string(), reason: "symbolic link".into() });
                    } else {
                        stack.push((path, rp));
                    }
                } else if meta.is_file() {
                    ev(Event::FileStart { remote: rp.clone(), local: path.clone(), size: Some(meta.len()) });
                    let known = Some(existing.get(&name).filter(|e| !e.is_dir()).map(info_of));
                    let r = self.upload_file(&path, &rp, known, opts, &mut *resolve, &mut |done| ev(Event::Progress { done })).await;
                    match r {
                        Ok(Outcome::Done { bytes, .. }) => ev(Event::FileDone { bytes }),
                        Ok(Outcome::Skipped) => ev(Event::Skipped { path: path.display().to_string(), reason: "target exists".into() }),
                        Err(e) => {
                            failed += 1;
                            ev(Event::Failed { path: path.display().to_string(), error: format!("{e:#}") });
                        }
                    }
                }
            }
        }
        if failed > 0 {
            bail!("{failed} file(s) failed to upload");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(basename("/a/b.txt"), "b.txt");
        assert_eq!(basename("bucket/dir/"), "dir");
        assert_eq!(parent("/a/b.txt"), "/a");
        assert_eq!(parent("/b.txt"), "/");
        assert_eq!(parent("bucket/key"), "bucket");
    }

    #[test]
    fn unsafe_names() {
        assert!(safe_name("ok.txt").is_ok());
        assert!(safe_name("..").is_err());
        assert!(safe_name("../etc/passwd").is_err());
        assert!(safe_name("a\\b").is_err());
    }
}

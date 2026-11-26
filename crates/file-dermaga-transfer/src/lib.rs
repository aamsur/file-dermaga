//! Backend-neutral file operations shared by the CLI and the GUI.
//!
//! Paths are strings: `/dir/file` for FTP, SFTP and WebDAV (WebDAV paths
//! are relative to the server's base URL), `bucket/key` for S3 (empty =
//! bucket list).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use file_dermaga_core::DirEntry;
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

    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        match self {
            Backend::Ftp(c) => Ok(c.rename(from, to).await?),
            Backend::S3(c) => c.rename(from, to).await,
            Backend::Sftp(c) => c.rename(from, to).await,
            Backend::Webdav(c) => c.rename(from, to).await,
        }
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

    /// Download a directory tree into `local`.
    pub async fn download_tree(&mut self, remote: &str, local: &Path, ev: &mut (dyn FnMut(Event) + Send)) -> Result<()> {
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
                let size = u64::try_from(e.size).ok();
                ev(Event::FileStart { remote: rp.clone(), local: lp.clone(), size });
                match self.download(&rp, &lp, &mut |done| ev(Event::Progress { done })).await {
                    Ok(n) => ev(Event::FileDone { bytes: n }),
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
    pub async fn upload_tree(&mut self, local: &Path, remote: &str, ev: &mut (dyn FnMut(Event) + Send)) -> Result<()> {
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
                    match self.upload(&path, &rp, &mut |done| ev(Event::Progress { done })).await {
                        Ok(n) => ev(Event::FileDone { bytes: n }),
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

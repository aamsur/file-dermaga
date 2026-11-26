//! Amazon S3 / S3-compatible backend (AWS, MinIO, Wasabi, Cloudflare R2,
//! Backblaze B2 S3 API, DigitalOcean Spaces, ...).
//!
//! Paths are `bucket/key`. The empty path is the account root (list of
//! buckets). "Directories" are key prefixes ending in `/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{
    BucketLocationConstraint, CompletedMultipartUpload, CompletedPart, CreateBucketConfiguration, Delete,
    ObjectIdentifier, ServerSideEncryption, StorageClass,
};
use aws_sdk_s3::Client;
use file_dermaga_core::{Accuracy, DateTime, DirEntry, FLAG_DIR};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;

/// Smallest part size S3 accepts (except for the last part).
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
/// S3 allows at most 10000 parts per upload.
const MAX_PARTS: u64 = 10_000;

#[derive(Clone, Debug, Default)]
pub struct S3Config {
    /// Custom endpoint for S3-compatible services, e.g. `http://localhost:9000`.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Named profile from `~/.aws/config` / `~/.aws/credentials`.
    pub profile: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    /// Path-style addressing (`endpoint/bucket/key`), needed by MinIO and
    /// most self-hosted services.
    pub path_style: bool,
    pub upload: UploadOptions,
}

#[derive(Clone, Debug)]
pub struct UploadOptions {
    /// e.g. `STANDARD`, `STANDARD_IA`, `GLACIER_IR`, `INTELLIGENT_TIERING`.
    pub storage_class: Option<String>,
    /// `AES256` or `aws:kms`.
    pub sse: Option<String>,
    pub sse_kms_key_id: Option<String>,
    /// Files larger than this are sent with multipart upload.
    pub multipart_threshold: u64,
    pub part_size: u64,
    /// Parts uploaded in parallel.
    pub concurrency: usize,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            storage_class: None,
            sse: None,
            sse_kms_key_id: None,
            multipart_threshold: 16 * 1024 * 1024,
            part_size: 16 * 1024 * 1024,
            concurrency: 4,
        }
    }
}

fn sdk<E: std::error::Error>(e: E) -> anyhow::Error {
    anyhow!("{}", DisplayErrorContext(e))
}

/// Split `bucket/key` into its parts.
pub fn split(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/');
    match path.split_once('/') {
        Some((b, k)) => (b, k),
        None => (path, ""),
    }
}

/// Turn a directory key into a listing prefix (`a/b` -> `a/b/`).
fn dir_prefix(key: &str) -> String {
    if key.is_empty() || key.ends_with('/') {
        key.to_owned()
    } else {
        format!("{key}/")
    }
}

fn to_datetime(t: Option<&aws_sdk_s3::primitives::DateTime>) -> DateTime {
    t.and_then(|t| t.to_millis().ok())
        .map(|ms| DateTime::from_unix_millis(ms, Accuracy::Seconds))
        .unwrap_or_default()
}

/// RFC 3986 encoding for `x-amz-copy-source` (keeps `/`).
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone)]
pub struct S3Client {
    client: Client,
    region: String,
    upload: UploadOptions,
}

impl S3Client {
    pub async fn connect(cfg: &S3Config) -> Result<Self> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(p) = &cfg.profile {
            loader = loader.profile_name(p);
        }
        if let Some(r) = &cfg.region {
            loader = loader.region(Region::new(r.clone()));
        }
        if let (Some(ak), Some(sk)) = (&cfg.access_key, &cfg.secret_key) {
            loader = loader.credentials_provider(Credentials::new(
                ak,
                sk,
                cfg.session_token.clone(),
                None,
                "file-dermaga",
            ));
        }
        let shared = loader.load().await;

        let mut b = aws_sdk_s3::config::Builder::from(&shared);
        if let Some(ep) = &cfg.endpoint {
            b = b.endpoint_url(ep);
        }
        // Signing needs a region even for services that ignore it.
        let region = shared.region().map(|r| r.to_string()).unwrap_or_else(|| "us-east-1".into());
        if shared.region().is_none() {
            b = b.region(Region::new(region.clone()));
        }
        b = b.force_path_style(cfg.path_style);

        Ok(Self { client: Client::from_conf(b.build()), region, upload: cfg.upload.clone() })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub async fn list_buckets(&self) -> Result<Vec<DirEntry>> {
        let out = self.client.list_buckets().send().await.map_err(sdk)?;
        Ok(out
            .buckets()
            .iter()
            .filter_map(|b| {
                Some(DirEntry {
                    name: b.name()?.to_owned(),
                    flags: FLAG_DIR,
                    time: to_datetime(b.creation_date()),
                    ..Default::default()
                })
            })
            .collect())
    }

    /// List one level below `path` (bucket list for the empty path).
    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let (bucket, key) = split(path);
        if bucket.is_empty() {
            return self.list_buckets().await;
        }
        let prefix = dir_prefix(key);
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .delimiter("/")
            .into_paginator()
            .send();

        let mut entries = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(sdk).with_context(|| format!("listing s3://{bucket}/{prefix}"))?;
            for p in page.common_prefixes() {
                if let Some(name) = p.prefix().and_then(|p| p.strip_prefix(&prefix)) {
                    entries.push(DirEntry {
                        name: name.trim_end_matches('/').to_owned(),
                        flags: FLAG_DIR,
                        ..Default::default()
                    });
                }
            }
            for o in page.contents() {
                let Some(name) = o.key().and_then(|k| k.strip_prefix(&prefix)) else {
                    continue;
                };
                // Skip the folder marker object of the listed prefix itself.
                if name.is_empty() {
                    continue;
                }
                entries.push(DirEntry {
                    name: name.to_owned(),
                    size: o.size().unwrap_or(-1),
                    time: to_datetime(o.last_modified()),
                    permissions: o.storage_class().map(|s| s.as_str().to_owned()).unwrap_or_default(),
                    ..Default::default()
                });
            }
        }
        Ok(entries)
    }

    /// All object keys below a prefix (recursive, no delimiter).
    async fn all_keys(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        let mut pages = self.client.list_objects_v2().bucket(bucket).prefix(prefix).into_paginator().send();
        let mut keys = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(sdk)?;
            keys.extend(page.contents().iter().filter_map(|o| o.key().map(str::to_owned)));
        }
        Ok(keys)
    }

    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        path: &str,
        out: &mut W,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let (bucket, key) = split(path);
        let resp = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk)
            .with_context(|| format!("downloading s3://{bucket}/{key}"))?;
        let mut body = resp.body;
        let mut total = 0u64;
        while let Some(chunk) = body.try_next().await? {
            out.write_all(&chunk).await?;
            total += chunk.len() as u64;
            progress(total);
        }
        out.flush().await?;
        Ok(total)
    }

    pub async fn upload(&self, local: &Path, path: &str, mut progress: impl FnMut(u64)) -> Result<u64> {
        let (bucket, key) = split(path);
        if bucket.is_empty() || key.is_empty() {
            bail!("upload target must be s3://bucket/key");
        }
        let size = tokio::fs::metadata(local).await?.len();
        let o = &self.upload;

        if size <= o.multipart_threshold {
            let mut req = self
                .client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from_path(local).await?);
            if let Some(sc) = &o.storage_class {
                req = req.storage_class(StorageClass::from(sc.as_str()));
            }
            if let Some(sse) = &o.sse {
                req = req.server_side_encryption(ServerSideEncryption::from(sse.as_str()));
            }
            if let Some(k) = &o.sse_kms_key_id {
                req = req.ssekms_key_id(k);
            }
            req.send().await.map_err(sdk).with_context(|| format!("uploading to s3://{bucket}/{key}"))?;
            progress(size);
            return Ok(size);
        }

        self.upload_multipart(local, bucket, key, size, progress).await
    }

    async fn upload_multipart(
        &self,
        local: &Path,
        bucket: &str,
        key: &str,
        size: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let o = &self.upload;
        // Grow the part size if the file would need more than 10000 parts.
        let part_size = o.part_size.max(MIN_PART_SIZE).max(size.div_ceil(MAX_PARTS));
        let parts = size.div_ceil(part_size);

        let mut req = self.client.create_multipart_upload().bucket(bucket).key(key);
        if let Some(sc) = &o.storage_class {
            req = req.storage_class(StorageClass::from(sc.as_str()));
        }
        if let Some(sse) = &o.sse {
            req = req.server_side_encryption(ServerSideEncryption::from(sse.as_str()));
        }
        if let Some(k) = &o.sse_kms_key_id {
            req = req.ssekms_key_id(k);
        }
        let created = req.send().await.map_err(sdk)?;
        let upload_id = created.upload_id().ok_or_else(|| anyhow!("server returned no upload id"))?.to_owned();

        let result = async {
            let mut done: Vec<CompletedPart> = Vec::with_capacity(parts as usize);
            let mut running: JoinSet<Result<(CompletedPart, u64)>> = JoinSet::new();
            let mut sent = 0u64;
            let mut next = 0u64;
            while next < parts || !running.is_empty() {
                while next < parts && running.len() < o.concurrency.max(1) {
                    let offset = next * part_size;
                    let len = part_size.min(size - offset);
                    let number = i32::try_from(next + 1)?;
                    let (client, bucket, key, id, file) = (
                        self.client.clone(),
                        bucket.to_owned(),
                        key.to_owned(),
                        upload_id.clone(),
                        PathBuf::from(local),
                    );
                    running.spawn(async move {
                        let body = ByteStream::read_from()
                            .path(file)
                            .offset(offset)
                            .length(Length::Exact(len))
                            .build()
                            .await?;
                        let out = client
                            .upload_part()
                            .bucket(bucket)
                            .key(key)
                            .upload_id(id)
                            .part_number(number)
                            .body(body)
                            .send()
                            .await
                            .map_err(sdk)?;
                        let part = CompletedPart::builder()
                            .part_number(number)
                            .set_e_tag(out.e_tag().map(str::to_owned))
                            .build();
                        Ok((part, len))
                    });
                    next += 1;
                }
                if let Some(res) = running.join_next().await {
                    let (part, len) = res??;
                    sent += len;
                    progress(sent);
                    done.push(part);
                }
            }
            done.sort_by_key(|p| p.part_number());
            self.client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build())
                .send()
                .await
                .map_err(sdk)?;
            Ok::<_, anyhow::Error>(sent)
        }
        .await;

        if result.is_err() {
            // Don't leave billable orphaned parts behind.
            let _ = self.client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
        }
        result.with_context(|| format!("multipart upload to s3://{bucket}/{key}"))
    }

    /// Create a bucket (`bucket`) or a folder marker (`bucket/dir`).
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let (bucket, key) = split(path);
        if bucket.is_empty() {
            bail!("no bucket name given");
        }
        if key.trim_end_matches('/').is_empty() {
            let mut req = self.client.create_bucket().bucket(bucket);
            if self.region != "us-east-1" {
                req = req.create_bucket_configuration(
                    CreateBucketConfiguration::builder()
                        .location_constraint(BucketLocationConstraint::from(self.region.as_str()))
                        .build(),
                );
            }
            req.send().await.map_err(sdk).with_context(|| format!("creating bucket {bucket}"))?;
            return Ok(());
        }
        self.client
            .put_object()
            .bucket(bucket)
            .key(dir_prefix(key))
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(sdk)?;
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let (bucket, key) = split(path);
        if key.is_empty() {
            bail!("refusing to delete a bucket with `rm`; use `rmdir s3://{bucket}`");
        }
        self.client.delete_object().bucket(bucket).key(key).send().await.map_err(sdk)?;
        Ok(())
    }

    /// Delete everything below a prefix; also deletes the bucket when the
    /// path is a bucket and `recursive` is set.
    pub async fn rmdir(&self, path: &str, recursive: bool) -> Result<()> {
        let (bucket, key) = split(path);
        let prefix = dir_prefix(key);
        if recursive {
            let keys = self.all_keys(bucket, &prefix).await?;
            for chunk in keys.chunks(1000) {
                let objects = chunk
                    .iter()
                    .map(|k| ObjectIdentifier::builder().key(k).build())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let out = self
                    .client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(Delete::builder().set_objects(Some(objects)).quiet(true).build()?)
                    .send()
                    .await
                    .map_err(sdk)?;
                if let Some(e) = out.errors().first() {
                    bail!(
                        "failed to delete {}: {}",
                        e.key().unwrap_or("?"),
                        e.message().unwrap_or("unknown error")
                    );
                }
            }
        } else if key.is_empty() {
            // Bucket must be empty; S3 reports BucketNotEmpty otherwise.
        } else {
            let keys = self.all_keys(bucket, &prefix).await?;
            if keys.iter().any(|k| *k != prefix) {
                bail!("s3://{bucket}/{prefix} is not empty (use -r)");
            }
            if !keys.is_empty() {
                self.client.delete_object().bucket(bucket).key(&prefix).send().await.map_err(sdk)?;
            }
        }
        if key.is_empty() {
            self.client.delete_bucket().bucket(bucket).send().await.map_err(sdk)?;
        }
        Ok(())
    }

    /// Server-side copy followed by delete (objects up to 5 GiB).
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (fb, fk) = split(from);
        let (tb, tk) = split(to);
        if fk.is_empty() || tk.is_empty() {
            bail!("rename needs s3://bucket/key on both sides");
        }
        // CopyObject resets storage class and encryption unless they are
        // given again, so carry them over from the source object.
        let head = self
            .client
            .head_object()
            .bucket(fb)
            .key(fk)
            .send()
            .await
            .map_err(sdk)
            .with_context(|| format!("reading s3://{fb}/{fk}"))?;
        self.client
            .copy_object()
            .copy_source(format!("{fb}/{}", encode_key(fk)))
            .bucket(tb)
            .key(tk)
            .set_storage_class(head.storage_class().cloned())
            .set_server_side_encryption(head.server_side_encryption().cloned())
            .set_ssekms_key_id(head.ssekms_key_id().map(str::to_owned))
            .send()
            .await
            .map_err(sdk)
            .with_context(|| format!("copying s3://{fb}/{fk}"))?;
        self.client.delete_object().bucket(fb).key(fk).send().await.map_err(sdk)?;
        Ok(())
    }

    /// Pre-signed GET URL, shareable without credentials until it expires.
    pub async fn presign(&self, path: &str, expires: Duration) -> Result<String> {
        let (bucket, key) = split(path);
        let req = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(PresigningConfig::expires_in(expires)?)
            .await
            .map_err(sdk)?;
        Ok(req.uri().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits() {
        assert_eq!(split(""), ("", ""));
        assert_eq!(split("b"), ("b", ""));
        assert_eq!(split("b/"), ("b", ""));
        assert_eq!(split("b/x/y.txt"), ("b", "x/y.txt"));
        assert_eq!(split("/b/x"), ("b", "x"));
    }

    #[test]
    fn copy_source_encoding() {
        assert_eq!(encode_key("dir/a b+c.txt"), "dir/a%20b%2Bc.txt");
    }
}

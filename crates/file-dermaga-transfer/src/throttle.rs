//! Global transfer speed limits (token buckets) and throttled I/O wrappers.
//!
//! Limits apply to the bytes written to / read from local files, so they
//! cover every protocol that streams through FileDermaga (FTP, SFTP,
//! WebDAV and S3 downloads). S3 uploads are read by the AWS SDK directly
//! from the file and are not limited.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

/// A token bucket; rate 0 means unlimited. The rate can change at any time.
#[derive(Debug)]
pub struct Bucket {
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl Default for Bucket {
    fn default() -> Self {
        Self { rate: AtomicU64::new(0), state: Mutex::new((0.0, Instant::now())) }
    }
}

impl Bucket {
    /// Bytes per second; 0 disables the limit.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// How many of `want` bytes may pass now, or how long to wait first.
    fn grant(&self, want: usize) -> Result<usize, Duration> {
        let rate = self.rate();
        if rate == 0 || want == 0 {
            return Ok(want);
        }
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        // Burst of up to a quarter second keeps throughput smooth.
        let cap = (rate as f64 / 4.0).max(1024.0);
        st.0 = (st.0 + now.duration_since(st.1).as_secs_f64() * rate as f64).min(cap);
        st.1 = now;
        if st.0 >= 1.0 {
            let n = want.min(st.0 as usize).max(1);
            st.0 -= n as f64;
            Ok(n)
        } else {
            Err(Duration::from_secs_f64(((1.0 - st.0) / rate as f64).max(0.001)))
        }
    }
}

/// Download and upload limits shared by all transfers.
#[derive(Clone, Debug, Default)]
pub struct Limiter {
    pub down: Arc<Bucket>,
    pub up: Arc<Bucket>,
}

impl Limiter {
    /// Limits in KiB/s; 0 = unlimited.
    pub fn set_kib(&self, down: u64, up: u64) {
        self.down.set_rate(down * 1024);
        self.up.set_rate(up * 1024);
    }
}

/// Wait out a pending throttle delay; `Ready` once it has elapsed.
fn poll_delay(sleep: &mut Option<Pin<Box<Sleep>>>, cx: &mut Context<'_>) -> Poll<()> {
    if let Some(s) = sleep.as_mut() {
        if s.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        *sleep = None;
    }
    Poll::Ready(())
}

pub struct ThrottledWriter<W> {
    inner: W,
    bucket: Arc<Bucket>,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<W> ThrottledWriter<W> {
    pub fn new(inner: W, bucket: Arc<Bucket>) -> Self {
        Self { inner, bucket, sleep: None }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ThrottledWriter<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            if poll_delay(&mut self.sleep, cx).is_pending() {
                return Poll::Pending;
            }
            match self.bucket.grant(buf.len()) {
                Ok(n) => return Pin::new(&mut self.inner).poll_write(cx, &buf[..n]),
                Err(wait) => self.sleep = Some(Box::pin(tokio::time::sleep(wait))),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct ThrottledReader<R> {
    inner: R,
    bucket: Arc<Bucket>,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<R> ThrottledReader<R> {
    pub fn new(inner: R, bucket: Arc<Bucket>) -> Self {
        Self { inner, bucket, sleep: None }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ThrottledReader<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            if poll_delay(&mut self.sleep, cx).is_pending() {
                return Poll::Pending;
            }
            match self.bucket.grant(buf.remaining()) {
                Ok(n) => {
                    let dst = buf.initialize_unfilled_to(n);
                    let mut part = ReadBuf::new(dst);
                    let r = Pin::new(&mut self.inner).poll_read(cx, &mut part);
                    let filled = part.filled().len();
                    buf.advance(filled);
                    return r;
                }
                Err(wait) => self.sleep = Some(Box::pin(tokio::time::sleep(wait))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn limits_write_rate() {
        let b = Arc::new(Bucket::default());
        b.set_rate(200 * 1024);
        let mut w = ThrottledWriter::new(Vec::new(), b);
        let t0 = Instant::now();
        w.write_all(&vec![0u8; 150 * 1024]).await.unwrap();
        let secs = t0.elapsed().as_secs_f64();
        // 150 KiB at 200 KiB/s with a 50 KiB burst: about 0.5 s.
        assert!((0.35..1.5).contains(&secs), "took {secs}s");
    }

    #[tokio::test]
    async fn unlimited_is_instant() {
        let mut w = ThrottledWriter::new(Vec::new(), Arc::new(Bucket::default()));
        let t0 = Instant::now();
        w.write_all(&vec![0u8; 4 << 20]).await.unwrap();
        assert!(t0.elapsed() < Duration::from_millis(500));
    }
}

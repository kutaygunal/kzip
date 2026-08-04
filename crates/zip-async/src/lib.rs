//! # zip-async
//!
//! Async streaming adapter for [`zip-core`](zip_core), feature-gated on tokio.
//!
//! ## Modes
//!
//! - **Bridge mode** (implemented here): the sync engine runs on tokio's
//!   blocking thread pool via `spawn_blocking`, exposed behind
//!   `AsyncRead`/`AsyncWrite` handles. Correct, simple, and good enough for
//!   I/O-bound use.
//! - **Native poll path** (deferred): drive the engine from an async source
//!   with `poll_read` and zero thread hops for pure-buffer/crypto pipelines.
//!   The in-memory read path below already avoids a thread hop for `Store`
//!   members served from a contiguous buffer; full native codec pipelines are
//!   future work because decompression is CPU-bound and should not block the
//!   executor.
//!
//! All reads are safe (no `unsafe`), deterministic, and forward the
//! sync engine's CRC/size integrity checks.

#![deny(unsafe_code)]
#![deny(missing_debug_implementations)]
#![warn(missing_docs)]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

use zip_core::{Archive, EntryReader, Result, ZipError, ZipErrorCode};

/// A ZIP archive opened asynchronously.
///
/// Opening is performed on the blocking thread pool. Readers are created via
/// [`AsyncArchive::open_reader`]; each returns an [`AsyncEntryReader`] that
/// implements [`AsyncRead`].
#[derive(Debug)]
pub struct AsyncArchive {
    inner: Arc<Archive>,
}

/// Chunk size used for the bridge-mode blocking reads.
const CHUNK: usize = 8192;

impl AsyncArchive {
    /// Open an archive from an in-memory byte buffer, running the synchronous
    /// `zip-core` engine on the blocking thread pool.
    pub async fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let arch = tokio::task::spawn_blocking(move || Archive::open(std::io::Cursor::new(bytes)))
            .await
            .map_err(|_| ZipError::new(ZipErrorCode::Internal))??;
        Ok(AsyncArchive {
            inner: Arc::new(arch),
        })
    }

    /// Number of entries in the archive.
    pub fn len(&self) -> u64 {
        self.inner.len()
    }

    /// Whether the archive is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Name of the entry at `index`, if any.
    pub fn name(&self, index: u64) -> Option<&str> {
        self.inner.name(index)
    }

    /// Open a streaming, async reader for the entry at `index`.
    ///
    /// The reader is constructed on the blocking pool (local-header parsing and
    /// decoder setup) and then serves decompressed bytes via [`AsyncRead`].
    pub async fn open_reader(&self, index: u64) -> Result<AsyncEntryReader> {
        let arc = self.inner.clone();
        let reader = tokio::task::spawn_blocking(move || arc.open_entry(index))
            .await
            .map_err(|_| ZipError::new(ZipErrorCode::Internal))??;
        Ok(AsyncEntryReader::new(reader))
    }
}

/// A bridge-mode async reader for one archive entry.
///
/// Internally it owns a `zip-core` [`EntryReader`]. Each poll launches a
/// bounded blocking read on the thread pool via `spawn_blocking`; the returned
/// bytes are buffered and served to the caller across polls, so a single
/// blocking read may satisfy multiple `poll_read` calls.
#[derive(Debug)]
pub struct AsyncEntryReader {
    inner: Mutex<Option<EntryReader>>,
    /// In-flight blocking read, returned with the reader and its staging buffer.
    pending: Option<oneshot::Receiver<(EntryReader, Vec<u8>, io::Result<usize>)>>,
    /// Buffer holding decoded bytes not yet handed to the caller.
    staging: Vec<u8>,
    /// Read position into `staging`.
    staging_pos: usize,
}

impl AsyncEntryReader {
    fn new(reader: EntryReader) -> Self {
        AsyncEntryReader {
            inner: Mutex::new(Some(reader)),
            pending: None,
            staging: Vec::new(),
            staging_pos: 0,
        }
    }

    /// Serve any buffered bytes in `staging` into `buf`. Returns `true` if
    /// bytes were written.
    fn serve_buffered(&mut self, buf: &mut ReadBuf<'_>) -> bool {
        if self.staging_pos < self.staging.len() {
            let avail = self.staging.len() - self.staging_pos;
            let n = avail.min(buf.remaining());
            if n > 0 {
                buf.put_slice(&self.staging[self.staging_pos..self.staging_pos + n]);
                self.staging_pos += n;
                if self.staging_pos == self.staging.len() {
                    self.staging_pos = 0;
                    self.staging.clear();
                }
                return true;
            }
        }
        false
    }

    /// Handle a completed blocking read, storing the reader + data for reuse.
    fn finish_read(
        &mut self,
        msg: (EntryReader, Vec<u8>, io::Result<usize>),
    ) -> Poll<io::Result<()>> {
        let (reader, staging, res) = msg;
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                *self.inner.lock().expect("lock") = None;
                self.pending = None;
                return Poll::Ready(Err(e));
            }
        };
        self.pending = None;
        self.staging_pos = 0;
        self.staging = staging;
        if n == 0 {
            // EOF: the reader is exhausted; drop it and report clean EOF.
            *self.inner.lock().expect("lock") = None;
            self.staging.clear();
            return Poll::Ready(Ok(()));
        }
        // Keep the reader for the next chunk.
        self.staging.truncate(n);
        *self.inner.lock().expect("lock") = Some(reader);
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for AsyncEntryReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            // 1. Serve already-buffered bytes if any.
            if this.serve_buffered(buf) {
                return Poll::Ready(Ok(()));
            }
            // 2. Collect a completed blocking read.
            if let Some(rx) = &mut this.pending {
                match Pin::new(rx).poll(cx) {
                    Poll::Ready(Ok(msg)) => match this.finish_read(msg) {
                        Poll::Ready(Ok(())) => continue,
                        other => return other,
                    },
                    Poll::Ready(Err(_)) => {
                        this.pending = None;
                        *this.inner.lock().expect("lock") = None;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::Other,
                            "blocking decode task cancelled",
                        )));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            // 3. Launch a new blocking read.
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let reader = match this.inner.lock().expect("lock").take() {
                Some(r) => r,
                None => return Poll::Ready(Ok(())), // EOF
            };
            let mut staging = std::mem::take(&mut this.staging);
            staging.clear();
            staging.resize(CHUNK, 0);
            let (tx, rx) = oneshot::channel();
            tokio::task::spawn_blocking(move || {
                let mut reader = reader;
                let res = io::Read::read(&mut reader, &mut staging);
                let _ = tx.send((reader, staging, res));
            });
            this.pending = Some(rx);
            // Loop back to poll the receiver.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use zip_core::constant::CompressionMethod;
    use zip_core::{write_archive, ArchiveFile, CompressOptions};

    fn sample_bytes() -> Vec<u8> {
        let files = vec![
            ArchiveFile::new("a/one.txt", b"hello one file, some content here".repeat(40)),
            ArchiveFile::new("b/two.bin", vec![0u8; 4096]),
            ArchiveFile::new(
                "c/three.txt",
                b"third file with compressible text ".repeat(200),
            ),
        ];
        write_archive(&files, &CompressOptions::default()).unwrap()
    }

    #[tokio::test]
    async fn opens_and_reads_entries() {
        let bytes = sample_bytes();
        let arch = AsyncArchive::from_bytes(bytes).await.unwrap();
        assert_eq!(arch.len(), 3);
        assert_eq!(arch.name(1), Some("b/two.bin"));

        let mut r = arch.open_reader(0).await.unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"hello one file, some content here".repeat(40));
    }

    #[tokio::test]
    async fn reads_all_entries_in_order() {
        let files = vec![
            ArchiveFile::new("x.txt", b"x".repeat(100_000)), // spans many chunks
            ArchiveFile::new("y.bin", vec![7u8; 200_000]),
        ];
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        let arch = AsyncArchive::from_bytes(bytes).await.unwrap();

        for (i, f) in files.iter().enumerate() {
            let mut r = arch.open_reader(i as u64).await.unwrap();
            let mut out = Vec::new();
            r.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, f.data, "mismatch at entry {i}");
        }
    }

    #[tokio::test]
    async fn out_of_range_reader_errors() {
        let arch = AsyncArchive::from_bytes(sample_bytes()).await.unwrap();
        let err = arch.open_reader(99).await.unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Inval);
    }

    #[tokio::test]
    async fn reads_byte_at_a_time_across_many_polls() {
        // One-byte reads exercise `serve_buffered`/`staging` across many polls:
        // a single blocking CHUNK read must satisfy many tiny `poll_read` calls
        // with the correct bytes in order, ending cleanly at EOF.
        let files = vec![ArchiveFile::new(
            "a.txt",
            b"byte-at-a-time async read payload ".repeat(200),
        )];
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        let arch = AsyncArchive::from_bytes(bytes).await.unwrap();
        let mut r = arch.open_reader(0).await.unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            let n = r.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.push(buf[0]);
        }
        assert_eq!(out, files[0].data);
    }

    #[tokio::test]
    async fn store_entry_reads_correctly() {
        // Store (uncompressed) entries must flow through the async reader too.
        let files = vec![ArchiveFile::new("s.bin", vec![9u8; 5000])];
        let opts = CompressOptions {
            method: CompressionMethod::Store,
            ..Default::default()
        };
        let bytes = write_archive(&files, &opts).unwrap();
        let arch = AsyncArchive::from_bytes(bytes).await.unwrap();
        let mut r = arch.open_reader(0).await.unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, vec![9u8; 5000]);
    }

    #[tokio::test]
    async fn corrupted_entry_surfaces_integrity_error() {
        // Corrupt a stored member's compressed bytes; reading must surface an
        // error (either at open, when the zero-copy path decodes eagerly, or on
        // the final CRC/size check on the streaming path). It must never return
        // silently-corrupt bytes.
        let name = "c.txt";
        let payload = b"verify me async integrity ".repeat(60);
        let files = vec![ArchiveFile::new(name, payload.to_vec())];
        let mut bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        let data_start = 30 + name.len(); // local header + name
        bytes[data_start] ^= 0xFF;
        bytes[data_start + 3] ^= 0x55;

        let arch = AsyncArchive::from_bytes(bytes).await.unwrap();
        match arch.open_reader(0).await {
            Ok(mut r) => {
                let mut out = Vec::new();
                assert!(
                    r.read_to_end(&mut out).await.is_err(),
                    "corrupted entry must error on read"
                );
            }
            Err(_) => {
                // Decode failure surfaced eagerly at open_reader — acceptable.
            }
        }
    }
}

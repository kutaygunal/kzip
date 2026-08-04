//! The read path: an `EntryReader` decompresses and CRC-verifies one member.
//!
//! Mirrors libzip's layered `zip_source_zip` (file → window → crc → compress):
//! a decoder streaming from a source windowed to `comp_size`, with a CRC layer
//! that verifies the uncompressed size and CRC-32 at EOF (like libzip's
//! `zip_source_crc` with `validate = true`).

use crate::bufferpool::BufferPool;
use crate::codec::Decoder;
use crate::constant::CompressionMethod;
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::source::Source;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

/// The inner read strategy for an [`EntryReader`].
///
/// - [`Inner::Streaming`] decodes lazily from the entry's `Source` as bytes are
///   pulled (used for non-contiguous sources, e.g. real files).
/// - [`Inner::Buffered`] holds an already-decoded, in-memory buffer produced by
///   the zero-copy read path ([`crate::codec::decode_slice_into`]) for
///   contiguous, buffer-backed sources. The buffer is borrowed from a
///   [`BufferPool`] and returned to it when the reader is dropped.
enum Inner {
    /// Streaming decoder fed by the entry source.
    Streaming(Decoder),
    /// Decoded bytes served from memory, plus the pool to return them to.
    Buffered {
        data: Vec<u8>,
        pos: usize,
        pool: Option<Arc<Mutex<BufferPool>>>,
    },
}

/// A streaming reader for a single archive entry's decompressed content.
///
/// Implements [`Read`]; on reaching EOF it verifies the uncompressed size and
/// CRC-32, returning [`ZipErrorCode::Crc`] on mismatch (mirroring libzip).
pub struct EntryReader {
    inner: Inner,
    crc: crc32fast::Hasher,
    expected_crc: u32,
    expected_size: u64,
    size_read: u64,
    /// True once EOF has been reached and the integrity check has run.
    finished: bool,
    /// Deferred integrity error to surface on the final read.
    integrity_err: Option<io::Error>,
}

impl std::fmt::Debug for EntryReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryReader")
            .field("expected_crc", &self.expected_crc)
            .field("expected_size", &self.expected_size)
            .field("size_read", &self.size_read)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl EntryReader {
    /// Build an `EntryReader` over `src` (positioned at the entry's data start).
    pub fn new(
        src: Box<dyn Source>,
        method: CompressionMethod,
        comp_size: u64,
        uncomp_size: u64,
        expected_crc: u32,
        encrypted: bool,
    ) -> Result<EntryReader> {
        if encrypted {
            // Decryption (PKWARE / WinZip AES) arrives in a later phase.
            return Err(ZipError::new(ZipErrorCode::Encrmethnotsupp));
        }
        let inner = Decoder::new(src, method, comp_size)?;
        Ok(EntryReader {
            inner: Inner::Streaming(inner),
            crc: crc32fast::Hasher::new(),
            expected_crc,
            expected_size: uncomp_size,
            size_read: 0,
            finished: false,
            integrity_err: None,
        })
    }

    /// Build an `EntryReader` over an already-decoded, in-memory buffer (the
    /// zero-copy fast path).
    ///
    /// `data` holds the entry's decompressed bytes; it is typically a buffer
    /// acquired from the archive's [`BufferPool`] (via `pool`), which will be
    /// returned to that pool when the reader is dropped.
    pub(crate) fn from_buffer(
        data: Vec<u8>,
        expected_crc: u32,
        expected_size: u64,
        pool: Option<Arc<Mutex<BufferPool>>>,
    ) -> EntryReader {
        EntryReader {
            inner: Inner::Buffered { data, pos: 0, pool },
            crc: crc32fast::Hasher::new(),
            expected_crc,
            expected_size,
            size_read: 0,
            finished: false,
            integrity_err: None,
        }
    }

    /// The entry's expected uncompressed size.
    pub fn expected_size(&self) -> u64 {
        self.expected_size
    }

    /// Bytes produced so far.
    pub fn position(&self) -> u64 {
        self.size_read
    }

    /// Whether the reader has consumed the full entry (integrity verified or errored).
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Check integrity at EOF. Returns `true` on success.
    fn verify(&mut self) -> bool {
        self.finished = true;
        if self.size_read != self.expected_size || self.crc.clone().finalize() != self.expected_crc
        {
            let err = io::Error::new(io::ErrorKind::InvalidData, "CRC or size mismatch");
            self.integrity_err = Some(err);
            false
        } else {
            true
        }
    }
}

impl Read for EntryReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(err) = self.integrity_err.take() {
            return Err(err);
        }
        let n = match &mut self.inner {
            Inner::Streaming(d) => d.read(buf)?,
            Inner::Buffered { data, pos, .. } => {
                if *pos < data.len() {
                    let take = (data.len() - *pos).min(buf.len());
                    buf[..take].copy_from_slice(&data[*pos..*pos + take]);
                    *pos += take;
                    take
                } else {
                    0
                }
            }
        };
        if n > 0 {
            self.crc.update(&buf[..n]);
            self.size_read += n as u64;
            Ok(n)
        } else {
            // EOF of the decompressed stream: verify size + CRC.
            if !self.finished {
                if !self.verify() {
                    // Surface the error on this read call.
                    if let Some(err) = self.integrity_err.take() {
                        return Err(err);
                    }
                }
            }
            Ok(0)
        }
    }
}

impl Drop for EntryReader {
    /// Return any pooled decode buffer to the archive's `BufferPool` so it can
    /// be reused on a later `open_entry` call (avoids per-entry allocation).
    fn drop(&mut self) {
        if let Inner::Buffered { data, pool, .. } = &mut self.inner {
            if let Some(pool) = pool {
                if let Ok(mut guard) = pool.lock() {
                    guard.release(std::mem::take(data));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn crc(data: &[u8]) -> u32 {
        let mut h = crc32fast::Hasher::new();
        h.update(data);
        h.finalize()
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn reads_and_verifies_ok() {
        let data = b"hello entry reader".repeat(30);
        let comp = deflate(&data);
        let clen = comp.len() as u64;
        let src: Box<dyn Source> = Box::new(Cursor::new(comp));
        let mut r = EntryReader::new(
            src,
            CompressionMethod::Deflate,
            clen,
            data.len() as u64,
            crc(&data),
            false,
        )
        .unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert!(r.finished());
    }

    #[test]
    fn crc_mismatch_errors() {
        let data: &[u8] = b"some content";
        let comp = deflate(data);
        let clen = comp.len() as u64;
        let src: Box<dyn Source> = Box::new(Cursor::new(comp));
        let mut r = EntryReader::new(
            src,
            CompressionMethod::Deflate,
            clen,
            data.len() as u64,
            0xDEADBEEF,
            false,
        )
        .unwrap();
        let mut out = Vec::new();
        let res = r.read_to_end(&mut out);
        assert!(res.is_err());
    }

    #[test]
    fn size_mismatch_errors() {
        let data: &[u8] = b"content";
        let comp = deflate(data);
        let clen = comp.len() as u64;
        let src: Box<dyn Source> = Box::new(Cursor::new(comp));
        // Wrong expected size.
        let mut r =
            EntryReader::new(src, CompressionMethod::Deflate, clen, 999, crc(data), false).unwrap();
        let mut out = Vec::new();
        assert!(r.read_to_end(&mut out).is_err());
    }

    #[test]
    fn encrypted_is_rejected() {
        let src: Box<dyn Source> = Box::new(Cursor::new(vec![0u8; 4]));
        let e = EntryReader::new(src, CompressionMethod::Store, 0, 0, 0, true).unwrap_err();
        assert_eq!(e.code(), ZipErrorCode::Encrmethnotsupp);
    }
}

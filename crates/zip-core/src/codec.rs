//! Compression-codec dispatch for the read path.
//!
//! Rather than reimplementing codecs (correctness risk, ~zero payoff), we bind
//! to audited, pure-Rust decoders: `flate2`/miniz_oxide for Deflate and
//! `bzip2-rs` for Bzip2. `Store` passes bytes through. The decoder is fed a
//! source limited to the entry's `comp_size` so it never spills into the data
//! descriptor or the next member.

use crate::constant::{CompressionMethod, MAX_DECOMPRESSED};
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::source::Source;
use std::io::{self, BufReader, Read};

/// A streaming decoder positioned at the start of an entry's compressed data.
//
// `large_enum_variant` is deliberately allowed: boxing the `Deflate`/`Bzip2`
// variants would add a heap allocation on the decode hot path and change the
// enum's layout. There is no correctness need to shrink it (constructed once
// per entry read), so we prefer the zero-allocation layout.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Decoder {
    /// Stored (uncompressed) entry.
    Store(io::Take<Box<dyn Source>>),
    /// Raw deflate (as used in ZIP, no zlib wrapper).
    Deflate(flate2::bufread::DeflateDecoder<BufReader<io::Take<Box<dyn Source>>>>),
    /// Bzip2.
    Bzip2(bzip2_rs::DecoderReader<io::Take<Box<dyn Source>>>),
}

impl Decoder {
    /// Build a decoder over `src` (positioned at data start) using `method`.
    ///
    /// `comp_size` bounds how many compressed bytes the decoder may consume.
    pub fn new(src: Box<dyn Source>, method: CompressionMethod, comp_size: u64) -> Result<Decoder> {
        let take = src.take(comp_size);
        match method {
            CompressionMethod::Store => Ok(Decoder::Store(take)),
            CompressionMethod::Deflate => {
                // P2: size the read buffer to the entry's compressed size
                // (capped at the 8 KiB default) instead of always allocating a
                // full 8 KiB buffer per entry. Tiny entries (the read_random
                // workload, ~512 B..=4 KiB) allocate only what they need;
                // large entries keep the 8 KiB buffer, so read_full throughput
                // is unchanged. Correctness is unaffected: a smaller buffer
                // merely means more underlying reads.
                let cap = (comp_size as usize).clamp(1, 8192);
                let dec = flate2::bufread::DeflateDecoder::new(BufReader::with_capacity(cap, take));
                Ok(Decoder::Deflate(dec))
            }
            CompressionMethod::Bzip2 => Ok(Decoder::Bzip2(bzip2_rs::DecoderReader::new(take))),
            CompressionMethod::Unsupported(_) => Err(ZipError::new(ZipErrorCode::Compnotsupp)),
        }
    }

    /// Read decompressed bytes into `buf`.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Decoder::Store(d) => d.read(buf),
            Decoder::Deflate(d) => d.read(buf),
            Decoder::Bzip2(d) => d.read(buf),
        }
    }
}

impl Read for Decoder {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Decoder::read(self, buf)
    }
}

/// Decode up to `comp_size` compressed bytes **directly from the caller-owned
/// slice** `data` (zero-copy source — the slice is borrowed, never copied).
/// Decompressed bytes are appended to `out` (after clearing it); `out` may be
/// reused from a [`crate::bufferpool::BufferPool`] to avoid per-call
/// allocation.
///
/// This mirrors `Decoder` but operates on an in-memory `&[u8]` rather than a
/// `Box<dyn Source>`, so a buffer-backed source that exposes its owned bytes
/// via [`Source::as_slice`] can be decoded
/// without any intermediate staging copy.
pub fn decode_slice_into(
    data: &[u8],
    method: CompressionMethod,
    comp_size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.clear();
    let bounded = &data[..(comp_size as usize).min(data.len())];
    // Zip-bomb guard: cap the actual decompressed output. `take(limit)` stops
    // the decoder after `limit` bytes, so a malicious small stream cannot
    // expand without bound. `limit` is one byte past the cap so we can detect
    // an over-limit stream (copied == limit) and reject it.
    let limit = MAX_DECOMPRESSED + 1;
    let copied = match method {
        CompressionMethod::Store => {
            std::io::copy(&mut bounded.take(comp_size).take(limit), out).map_err(io_to_zip)?
        }
        CompressionMethod::Deflate => {
            let cap = (comp_size as usize).clamp(1, 8192);
            let dec = flate2::bufread::DeflateDecoder::new(BufReader::with_capacity(
                cap,
                bounded.take(comp_size),
            ));
            std::io::copy(&mut dec.take(limit), out).map_err(io_to_zip)?
        }
        CompressionMethod::Bzip2 => {
            let dec = bzip2_rs::DecoderReader::new(bounded.take(comp_size));
            std::io::copy(&mut dec.take(limit), out).map_err(io_to_zip)?
        }
        CompressionMethod::Unsupported(_) => {
            return Err(ZipError::new(ZipErrorCode::Compnotsupp));
        }
    };
    if copied > MAX_DECOMPRESSED {
        // Trim the buffer back to the cap so it never holds more than the
        // allowed decompressed size, then reject the stream.
        out.truncate(MAX_DECOMPRESSED as usize);
        return Err(ZipError::new(ZipErrorCode::DecompressionLimit));
    }
    Ok(())
}

/// Map an IO error from the decode path onto a ZIP error, marking compressed
/// data as invalid (a decode failure on a bounded slice is a data error).
fn io_to_zip(e: io::Error) -> ZipError {
    ZipError::with_system(ZipErrorCode::CompressedData, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    /// Produce raw-deflate bytes for `data` using flate2.
    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn store_passthrough() {
        let src: Box<dyn Source> = Box::new(Cursor::new(vec![1, 2, 3]));
        let mut d = Decoder::new(src, CompressionMethod::Store, 3).unwrap();
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn deflate_decodes() {
        let data = b"the quick brown fox jumps over the lazy dog".repeat(20);
        let comp = deflate(&data);
        let src: Box<dyn Source> = Box::new(Cursor::new(comp.clone()));
        let mut d = Decoder::new(src, CompressionMethod::Deflate, comp.len() as u64).unwrap();
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn decode_slice_matches_decoder() {
        // Zero-copy decode from a borrowed slice must equal the streaming
        // Decoder byte-for-byte, for both Store and Deflate.
        for (data, method) in [
            (
                b"plain stored bytes that need no compression".to_vec(),
                CompressionMethod::Store,
            ),
            (
                b"compress me compress me compress me".repeat(50),
                CompressionMethod::Deflate,
            ),
        ] {
            let comp = if method == CompressionMethod::Deflate {
                deflate(&data)
            } else {
                data.clone()
            };
            let src: Box<dyn Source> = Box::new(Cursor::new(comp.clone()));
            let mut d = Decoder::new(src, method, comp.len() as u64).unwrap();
            let mut expected = Vec::new();
            d.read_to_end(&mut expected).unwrap();

            let mut actual = Vec::new();
            decode_slice_into(&comp, method, comp.len() as u64, &mut actual).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(actual, data);
        }
    }

    #[test]
    fn decode_slice_unsupported_method_errors() {
        let mut out = Vec::new();
        let err = decode_slice_into(&[0u8; 4], CompressionMethod::Unsupported(99), 4, &mut out)
            .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Compnotsupp);
    }

    /// A small deflate stream that expands beyond the decompression cap must be
    /// rejected with `DecompressionLimit` (zip-bomb guard), and the output
    /// buffer must not grow past the cap.
    #[test]
    fn decode_slice_rejects_expansion_beyond_cap() {
        // Highly compressible payload that decompresses to just over the cap.
        let payload = vec![0u8; (MAX_DECOMPRESSED + 1) as usize];
        let comp = deflate(&payload);
        // Sanity: the compressed stream is tiny relative to the cap.
        assert!((comp.len() as u64) < MAX_DECOMPRESSED / 100);

        let mut out = Vec::new();
        let err = decode_slice_into(
            &comp,
            CompressionMethod::Deflate,
            comp.len() as u64,
            &mut out,
        )
        .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::DecompressionLimit);
        // The output must not have been allowed to grow past the cap.
        assert!(out.len() as u64 <= MAX_DECOMPRESSED);
    }

    /// A deflate stream that decompresses to exactly the cap is still accepted
    /// (legitimate inputs are not broken).
    #[test]
    fn decode_slice_accepts_exactly_at_cap() {
        let payload = vec![0u8; MAX_DECOMPRESSED as usize];
        let comp = deflate(&payload);
        let mut out = Vec::new();
        decode_slice_into(
            &comp,
            CompressionMethod::Deflate,
            comp.len() as u64,
            &mut out,
        )
        .unwrap();
        assert_eq!(out.len() as u64, MAX_DECOMPRESSED);
    }
}

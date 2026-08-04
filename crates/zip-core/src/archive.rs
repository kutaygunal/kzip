//! The public archive type: opening, enumerating, and reading members.
//!
//! Mirrors the core of libzip's `zip_open`/`zip_get_num_entries`/
//! `zip_get_name`/`zip_fopen`/`zip_stat` surface for the read path.

use crate::bufferpool::BufferPool;
use crate::cdir::read_central_dir;
use crate::codec::decode_slice_into;
use crate::constant::{CompressionMethod, BUFFER_POOL_CAPACITY, ZERO_COPY_MAX_UNCOMP};
use crate::dirent::Dirent;
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::file::EntryReader;
use crate::source::{Source, Stat};
use std::io::SeekFrom;
use std::sync::{Arc, Mutex};

/// A ZIP archive opened for reading.
///
/// Owns the underlying source plus the parsed central directory. Entry readers
/// are created via [`Archive::open_entry`], each on its own duplicated source
/// handle.
pub struct Archive {
    src: Box<dyn Source>,
    entries: Vec<Dirent>,
    comment: String,
    is_zip64: bool,
    /// Reusable decode buffers for the zero-copy read path, shared with the
    /// `EntryReader`s it produces.
    pool: Arc<Mutex<BufferPool>>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("entry_count", &self.entries.len())
            .field("comment", &self.comment)
            .field("is_zip64", &self.is_zip64)
            .finish_non_exhaustive()
    }
}

impl Archive {
    /// Open an archive from any seekable source.
    pub fn open<S: Source + 'static>(src: S) -> Result<Archive> {
        let mut boxed: Box<dyn Source> = Box::new(src);
        let cd = read_central_dir(&mut boxed)?;
        Ok(Archive {
            src: boxed,
            entries: cd.entries,
            comment: cd.comment,
            is_zip64: cd.is_zip64,
            pool: Arc::new(Mutex::new(BufferPool::new(BUFFER_POOL_CAPACITY))),
        })
    }

    /// Number of entries in the archive.
    pub fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    /// Whether the archive has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Archive (EOCD) comment.
    pub fn comment(&self) -> &str {
        &self.comment
    }

    /// Whether the central directory used the ZIP64 EOCD.
    pub fn is_zip64(&self) -> bool {
        self.is_zip64
    }

    /// Name of the entry at `index`, or `None` if out of range.
    pub fn name(&self, index: u64) -> Option<&str> {
        self.entries
            .get(index as usize)
            .map(|d| d.filename.as_str())
    }

    /// Find the index of the first entry named `name`, or `None`.
    pub fn name_locate(&self, name: &str) -> Option<u64> {
        self.entries
            .iter()
            .position(|d| d.filename == name)
            .map(|i| i as u64)
    }

    /// Raw `Dirent` for `index` (borrows the archive).
    pub fn dirent(&self, index: u64) -> Option<&Dirent> {
        self.entries.get(index as usize)
    }

    /// Stat metadata for the entry at `index`.
    pub fn stat(&self, index: u64) -> Result<Stat> {
        let d = self
            .entries
            .get(index as usize)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Inval))?;
        Ok(Stat {
            index: Some(index),
            name: Some(d.filename.clone()),
            size: Some(d.uncomp_size),
            comp_size: Some(d.comp_size),
            mtime: Some(dos_to_unix(d.last_mod_time, d.last_mod_date)),
            crc: Some(d.crc),
            comp_method: Some(method_to_u16(d.comp_method)),
            encryption_method: Some(d.encryption_method),
            valid: 0xFFFFFFFF,
        })
    }

    /// Open a streaming reader for the entry at `index` (decompressed).
    pub fn open_entry(&self, index: u64) -> Result<EntryReader> {
        let d = self
            .entries
            .get(index as usize)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Inval))?;
        self.open_dirent(d)
    }

    /// Open a streaming reader for the first entry named `name`.
    pub fn open_by_name(&self, name: &str) -> Result<EntryReader> {
        let idx = self
            .name_locate(name)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Noent))?;
        self.open_entry(idx)
    }

    fn open_dirent(&self, d: &Dirent) -> Result<EntryReader> {
        let mut dup = self.src.duplicate()?;
        dup.seek(SeekFrom::Start(d.offset))
            .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
        // Parse the local header; this also advances to the data start.
        let (_local, _header_len) = Dirent::parse_local(&mut dup)?;
        let encrypted = d.bitflags & crate::constant::flag::ENCRYPTED != 0;
        let comp_size = d.comp_size;
        let uncomp_size = d.uncomp_size;
        let crc = d.crc;
        let method = d.comp_method;

        // Zero-copy fast path: for contiguous, buffer-backed sources we decode
        // the whole entry directly from the borrowed `&[u8]` slice (no copy of
        // the compressed bytes) into a pooled buffer. We skip this for
        // encrypted / unsupported entries and for very large entries, which
        // stream to keep memory bounded.
        if !encrypted
            && !matches!(method, CompressionMethod::Unsupported(_))
            && uncomp_size <= ZERO_COPY_MAX_UNCOMP
        {
            let pos = dup
                .stream_position()
                .map_err(|e| ZipError::with_system(ZipErrorCode::Tell, e))?
                as usize;
            if let Some(slice) = dup.as_slice() {
                let end = (pos + comp_size as usize).min(slice.len());
                if pos <= end {
                    let data = &slice[pos..end];
                    // Grab a pooled buffer, release the lock, then decode.
                    let mut decoded = self
                        .pool
                        .lock()
                        .map_err(|_| ZipError::new(ZipErrorCode::Internal))?
                        .acquire();
                    decode_slice_into(data, method, comp_size, &mut decoded)?;
                    return Ok(EntryReader::from_buffer(
                        decoded,
                        crc,
                        uncomp_size,
                        Some(self.pool.clone()),
                    ));
                }
            }
        }

        // Streaming fallback: non-contiguous source (e.g. a real file), an
        // entry that is encrypted/unsupported, or one above the zero-copy size
        // cap.
        EntryReader::new(dup, method, comp_size, uncomp_size, crc, encrypted)
    }
}

fn method_to_u16(m: CompressionMethod) -> u16 {
    match m {
        CompressionMethod::Store => 0,
        CompressionMethod::Deflate => 8,
        CompressionMethod::Bzip2 => 12,
        CompressionMethod::Unsupported(v) => v as u16,
    }
}

/// Convert a DOS date/time to a Unix timestamp (seconds since epoch).
fn dos_to_unix(dos_time: u16, dos_date: u16) -> u64 {
    let secs = (dos_time & 0x1F) as u32 * 2;
    let mins = ((dos_time >> 5) & 0x3F) as u32;
    let hours = ((dos_time >> 11) & 0x1F) as u32;
    let day = (dos_date & 0x1F) as u32;
    let month = ((dos_date >> 5) & 0x0F) as u32;
    let year = ((dos_date >> 9) & 0x7F) as u32 + 1980;
    if year < 1970 || month == 0 || month > 12 || day == 0 {
        return 0;
    }
    // Days since epoch, via civil-from-days approximation.
    let days = days_from_civil(year as i64, month as i64, day as i64);
    if days < 0 {
        return 0;
    }
    (days as u64 * 86400) + (hours as u64 * 3600) + (mins as u64 * 60) + secs as u64
}

/// Howard Hinnant's `days_from_civil`: days from 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::{write_archive, ArchiveFile, CompressOptions};
    use crate::constant::magic;
    use std::io::{Cursor, Read};

    fn build_archive(filename: &str, content: &[u8]) -> Vec<u8> {
        let name = filename.as_bytes();
        let crc = crc32fast::hash(content);
        let mut v = Vec::new();
        v.extend_from_slice(&magic::LOCAL);
        v.extend_from_slice(&[20, 0, 0, 0]);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]);
        v.extend_from_slice(&crc.to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(name);
        // Local file header starts at offset 0.
        v.extend_from_slice(content);
        let cdir_offset = v.len() as u64;
        v.extend_from_slice(&magic::CENTRAL);
        v.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]);
        v.extend_from_slice(&crc.to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // offset = 0 (local header)
        v.extend_from_slice(name);
        let cdir_size = (v.len() - cdir_offset as usize) as u64;
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v
    }

    #[test]
    fn open_and_read_entry() {
        let bytes = build_archive("greet.txt", b"hello archive");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        assert_eq!(arch.len(), 1);
        assert_eq!(arch.name(0), Some("greet.txt"));
        assert_eq!(arch.stat(0).unwrap().size, Some(13));

        let mut r = arch.open_entry(0).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello archive");
    }

    #[test]
    fn open_by_name_and_missing() {
        let bytes = build_archive("a.txt", b"aaa");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        assert!(arch.open_by_name("a.txt").is_ok());
        assert_eq!(
            arch.open_by_name("nope.txt").unwrap_err().code(),
            ZipErrorCode::Noent
        );
    }

    /// A `Source` that deliberately hides its contiguous buffer (returns `None`
    /// from `as_slice`) so `Archive::open_dirent` is forced to take the
    /// streaming decoder path instead of the zero-copy path.
    struct NoSlice(Cursor<Vec<u8>>);

    impl std::io::Read for NoSlice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl std::io::Seek for NoSlice {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl Source for NoSlice {
        fn supports(&self) -> crate::source::Supports {
            crate::source::Supports::ReadableAndSeekable
        }
        fn as_slice(&self) -> Option<&[u8]> {
            None
        }
        fn duplicate(&self) -> Result<Box<dyn Source>> {
            Ok(Box::new(NoSlice(Cursor::new(self.0.get_ref().clone()))))
        }
    }

    /// The zero-copy buffered read path and the streaming decoder path must
    /// produce byte-identical output for the same entry.
    #[test]
    fn zero_copy_path_matches_streaming_path() {
        use crate::compress::{write_archive, ArchiveFile, CompressOptions};

        let files = vec![
            ArchiveFile::new(
                "a/one.txt",
                b"zero-copy versus streaming payload ".repeat(64),
            ),
            ArchiveFile::new("b/two.bin", vec![3u8; 4096]),
            ArchiveFile::new("c/three.txt", b"compress me compress me ".repeat(200)),
        ];
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();

        // Zero-copy path: contiguous `Cursor<Vec<u8>>` exposes `as_slice()`.
        let arch_zc = Archive::open(Cursor::new(bytes.clone())).unwrap();
        // Streaming path: a source that declines `as_slice()`.
        let arch_stream = Archive::open(NoSlice(Cursor::new(bytes))).unwrap();

        for i in 0..files.len() as u64 {
            let mut rzc = arch_zc.open_entry(i).unwrap();
            let mut zc = Vec::new();
            rzc.read_to_end(&mut zc).unwrap();

            let mut rs = arch_stream.open_entry(i).unwrap();
            let mut st = Vec::new();
            rs.read_to_end(&mut st).unwrap();

            assert_eq!(zc, st, "zero-copy and streaming disagree on entry {i}");
            assert_eq!(zc, files[i as usize].data);
        }
    }

    /// Build a Store (uncompressed) archive from `(name, content)` pairs.
    fn store_archive(contents: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let files = contents
            .iter()
            .map(|(n, d)| ArchiveFile::new(*n, d.clone()))
            .collect::<Vec<_>>();
        let opts = CompressOptions {
            method: CompressionMethod::Store,
            ..Default::default()
        };
        write_archive(&files, &opts).unwrap()
    }

    /// Zero-copy and streaming paths must agree byte-for-byte across a range of
    /// entry sizes, all well under the zero-copy size cap.
    #[test]
    fn zero_copy_byte_identity_across_entry_sizes() {
        let sizes = [1usize, 7, 255, 4096, 65536, 1 << 20];
        let files = sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let mut d = vec![0u8; n];
                d[i] = (i as u8).wrapping_add(1); // distinct content per entry
                ArchiveFile::new(format!("s{i}.bin"), d)
            })
            .collect::<Vec<_>>();
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        let zc = Archive::open(Cursor::new(bytes.clone())).unwrap();
        let st = Archive::open(NoSlice(Cursor::new(bytes))).unwrap();
        for i in 0..files.len() as u64 {
            let mut a = Vec::new();
            zc.open_entry(i).unwrap().read_to_end(&mut a).unwrap();
            let mut b = Vec::new();
            st.open_entry(i).unwrap().read_to_end(&mut b).unwrap();
            assert_eq!(a, b, "zero-copy/streaming disagree for entry {i}");
            assert_eq!(a, files[i as usize].data, "entry {i} content corrupted");
        }
    }

    /// A stored entry must read identically on the zero-copy path.
    #[test]
    fn zero_copy_store_entry_matches_streaming() {
        let bytes = store_archive(&[("s.txt", b"stored zero-copy entry".to_vec())]);
        let zc = Archive::open(Cursor::new(bytes.clone())).unwrap();
        let st = Archive::open(NoSlice(Cursor::new(bytes))).unwrap();
        let mut a = Vec::new();
        zc.open_entry(0).unwrap().read_to_end(&mut a).unwrap();
        let mut b = Vec::new();
        st.open_entry(0).unwrap().read_to_end(&mut b).unwrap();
        assert_eq!(a, b"stored zero-copy entry");
        assert_eq!(a, b);
    }

    /// The zero-copy cap is inclusive: an entry exactly at `ZERO_COPY_MAX_UNCOMP`
    /// still uses the zero-copy path and must match streaming.
    #[test]
    fn zero_copy_cap_boundary_is_inclusive() {
        let n = ZERO_COPY_MAX_UNCOMP as usize;
        let bytes = store_archive(&[("boundary.bin", vec![0xAB; n])]);
        let zc = Archive::open(Cursor::new(bytes.clone())).unwrap();
        let st = Archive::open(NoSlice(Cursor::new(bytes))).unwrap();
        let mut a = Vec::new();
        zc.open_entry(0).unwrap().read_to_end(&mut a).unwrap();
        let mut b = Vec::new();
        st.open_entry(0).unwrap().read_to_end(&mut b).unwrap();
        assert_eq!(a.len(), n);
        assert_eq!(a, b, "zero-copy at the size cap must match streaming");
    }

    /// An entry above `ZERO_COPY_MAX_UNCOMP` must fall back to the streaming
    /// decoder even on a contiguous (as_slice-capable) source, and still read
    /// every byte correctly.
    #[test]
    fn oversized_entry_uses_streaming_fallback_and_reads_fully() {
        let n = ZERO_COPY_MAX_UNCOMP as usize + 4096;
        let bytes = store_archive(&[("oversized.bin", vec![0xCD; n])]);
        let zc = Archive::open(Cursor::new(bytes)).unwrap();
        let mut a = Vec::new();
        zc.open_entry(0).unwrap().read_to_end(&mut a).unwrap();
        assert_eq!(a.len(), n);
        assert!(
            a.iter().all(|&b| b == 0xCD),
            "streaming fallback content corrupted"
        );
    }
}

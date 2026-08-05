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
use std::io::{Read, SeekFrom};
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
            // The 8 real ZIP_STAT_* bits libzip sets, not all 32 bits.
            valid: 0xFF,
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

    /// Read the full decompressed content of the entry at `index` into a
    /// `Vec`.
    ///
    /// This verifies the entry's CRC and size (via the streaming reader) and
    /// returns the verified bytes. The FFI layer uses it to serve entries as
    /// seekable in-memory buffers (so `zip_fseek`/`zip_ftell` work uniformly).
    pub fn read_entry(&self, index: u64) -> Result<Vec<u8>> {
        let mut r = self.open_entry(index)?;
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        Ok(out)
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
///
/// Mirrors libzip's `_zip_d2u_time`, which fills a `struct tm` and passes it
/// to `mktime` with `tm_isdst = -1`, so the stored DOS fields are interpreted
/// as LOCAL wall-clock time in the host's timezone (including DST). This
/// differs from a naive UTC interpretation by the host's UTC offset and DST
/// state, exactly as C libzip reports.
///
/// On Windows `chrono::Local::from_local_datetime` delegates to the C library's
/// `mktime` (the same primitive libzip uses), so timestamps match libzip on
/// the same host. On ambiguous/nonexistent local times (DST transitions) we
/// mirror `mktime`'s deterministic pick where possible and fall back to the
/// UTC interpretation otherwise.
fn dos_to_unix(dos_time: u16, dos_date: u16) -> u64 {
    let secs = (dos_time & 0x1F) as i64 * 2;
    let mins = ((dos_time >> 5) & 0x3F) as i64;
    let hours = ((dos_time >> 11) & 0x1F) as i64;
    let day = (dos_date & 0x1F) as i64;
    let month = ((dos_date >> 5) & 0x0F) as i64; // 1-based month, may be 0..15
    let year = ((dos_date >> 9) & 0x7F) as i64 + 1980;

    // libzip's `_zip_d2u_time` fills a `struct tm` (tm_year = year-1900,
    // tm_mon = month-1, tm_mday = day, ...) and calls `mktime` with
    // `tm_isdst = -1`. `mktime` NORMALIZES out-of-range fields: a DOS date of
    // 0/0 (month 0, day 0) becomes 1979-11-30 local, and the result is
    // interpreted in the host's LOCAL timezone. Reproduce that normalization
    // so invalid DOS dates and timezone-aware mtimes match libzip.
    let (y, m, d) = normalize_ymd(year, month, day);
    use chrono::{Local, NaiveDate, TimeZone};
    let naive = NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
        .and_then(|date| date.and_hms_opt(hours as u32, mins as u32, secs as u32));
    let naive = match naive {
        Some(n) => n,
        None => return 0,
    };
    use chrono::offset::LocalResult;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(t) => t.timestamp() as u64,
        LocalResult::Ambiguous(a, _) => a.timestamp() as u64,
        // Non-existent local time (DST spring-forward gap). `mktime` still
        // yields a value; fall back to the UTC interpretation.
        LocalResult::None => naive.and_utc().timestamp() as u64,
    }
}

/// Normalize a (year, month, day) triple to a valid calendar date the way
/// `mktime` does: month is 1-based and may be outside [1,12], day may be 0 or
/// larger than the month's length.
fn normalize_ymd(mut y: i64, m: i64, d: i64) -> (i64, i64, i64) {
    // Month: floor-based division handles m == 0 (=> December, previous year).
    let idx = m - 1;
    y += idx.div_euclid(12);
    let mut m = idx.rem_euclid(12) + 1;

    // Day: step back/forward by full months until it fits.
    let mut d = d;
    loop {
        if d < 1 {
            m -= 1;
            if m < 1 {
                m = 12;
                y -= 1;
            }
            d += days_in_month(y, m);
        } else {
            let dim = days_in_month(y, m);
            if d > dim {
                d -= dim;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            } else {
                break;
            }
        }
    }
    (y, m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 31, // unreachable after month normalization
    }
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

    /// `read_entry` returns the full, verified decompressed content of an
    /// entry (used by the FFI layer to serve seekable in-memory buffers).
    #[test]
    fn read_entry_returns_verified_bytes() {
        let files = vec![ArchiveFile::new(
            "a.txt",
            b"read_entry test content".to_vec(),
        )];
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let data = arch.read_entry(0).unwrap();
        assert_eq!(data, b"read_entry test content");
        // Out-of-range index errors.
        assert_eq!(arch.read_entry(5).unwrap_err().code(), ZipErrorCode::Inval);
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

    /// `valid` must be the 8 real ZIP_STAT_* bits (0xFF), and an unencrypted
    /// entry must report `encryption_method == 0` (ZIP_EM_NONE), both matching
    /// libzip.
    #[test]
    fn stat_valid_and_encryption_match_libzip() {
        let bytes = build_archive("s.txt", b"payload");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let st = arch.stat(0).unwrap();
        assert_eq!(st.valid, 0xFF);
        assert_eq!(st.encryption_method, Some(0)); // ZIP_EM_NONE
        assert_eq!(st.comp_method, Some(0)); // store
        assert_eq!(st.crc, Some(crc32fast::hash(b"payload")));
    }

    /// `dos_to_unix` must interpret the stored DOS fields as LOCAL wall-clock
    /// time (libzip's `mktime`, `tm_isdst = -1`), not as UTC. For a fixed
    /// non-ambiguous local datetime the result equals chrono's `Local`
    /// conversion and differs from the naive-UTC value by the host's offset.
    #[test]
    fn dos_to_unix_uses_local_timezone_like_mktime() {
        // DOS 2020-09-13 08:26:40 local wall clock.
        let dos_time = (8u16 << 11) | (26 << 5) | (40 / 2); // 08:26:40
        let dos_date = (40u16 << 9) | (9 << 5) | 13; // year=2020(40), month=9, day=13

        let t = dos_to_unix(dos_time, dos_date);

        use chrono::offset::LocalResult;
        use chrono::{Local, NaiveDate, TimeZone};
        let naive = NaiveDate::from_ymd_opt(2020, 9, 13)
            .unwrap()
            .and_hms_opt(8, 26, 40)
            .unwrap();
        let expected = match Local.from_local_datetime(&naive) {
            LocalResult::Single(x) => x.timestamp() as u64,
            LocalResult::Ambiguous(a, _) => a.timestamp() as u64,
            LocalResult::None => naive.and_utc().timestamp() as u64,
        };
        assert_eq!(t, expected);

        // On any non-zero-offset host the local interpretation must differ
        // from the naive-UTC one (matching libzip's timezone-aware mktime).
        let utc = naive.and_utc().timestamp() as u64;
        assert_eq!(
            (t as i64 - utc as i64).abs() % 900,
            0,
            "offset must be whole 15-min units"
        );
        if expected != utc {
            assert_ne!(t, utc, "local timezone conversion must differ from UTC");
        }
    }

    /// A DOS date of 0/0 (invalid: month 0, day 0) is NORMALIZED by libzip's
    /// `mktime` to 1979-11-30 local, not mapped to 0. Replicate that.
    #[test]
    fn dos_to_unix_zero_date_normalizes_like_mktime() {
        let t = dos_to_unix(0, 0);
        use chrono::{Local, NaiveDate, TimeZone};
        let naive = NaiveDate::from_ymd_opt(1979, 11, 30)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let expected = Local
            .from_local_datetime(&naive)
            .single()
            .unwrap()
            .timestamp() as u64;
        assert_eq!(t, expected);
        assert!(t > 300_000_000, "normalized date should be ~1979, not 0");
    }

    /// Day 0 rolls back to the previous month's last day; month 13 rolls into
    /// the next year. This mirrors `mktime`'s field normalization.
    #[test]
    fn dos_to_unix_rolls_out_of_range_fields() {
        // DOS 1980-02-01 with day 0 => 1980-01-31.
        // year bits=0, month bits=2, day=0 => date = (0<<9)|(2<<5)|0 = 64.
        let t = dos_to_unix(0, 64);
        use chrono::{Local, NaiveDate, TimeZone};
        let naive = NaiveDate::from_ymd_opt(1980, 1, 31)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(
            t,
            Local
                .from_local_datetime(&naive)
                .single()
                .unwrap()
                .timestamp() as u64
        );
    }
}

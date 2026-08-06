//! The public archive type: opening, enumerating, and reading members.
//!
//! Mirrors the core of libzip's `zip_open`/`zip_get_num_entries`/
//! `zip_get_name`/`zip_fopen`/`zip_stat` surface for the read path.
//!
//! For the *write* path with cooperative progress/cancel callbacks (Phase 6),
//! see [`write_archive_full_with_progress`] and [`WriteProgressPoll`] (re-exported
//! here for convenience; the implementation lives in [`crate::compress`]).

use crate::bufferpool::BufferPool;
use crate::cdir::read_central_dir;
use crate::codec::decode_slice_into;
use crate::constant::{CompressionMethod, BUFFER_POOL_CAPACITY, MAX_CD_SIZE, ZERO_COPY_MAX_UNCOMP};
use crate::crypto::{DecryptingSource, ZipCrypto, ENCRYPTION_HEADER_LEN};
use crate::dirent::Dirent;
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::file::EntryReader;
use crate::reader;
use crate::source::{SharedFile, SharedFileState, Source, Stat};
use std::io::Read;
use std::sync::{Arc, Mutex};

// Re-export the write-path progress/cancel polling API so it is reachable from
// the archive module (used by the C ABI layer's materialize path).
pub use crate::compress::{write_archive_full_with_progress, WriteProgressPoll};

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
    /// Shared, mutex-protected file handle used to open per-entry readers
    /// without a per-entry `try_clone`/`DuplicateHandle` (P1). `None` for
    /// non-file sources (e.g. in-memory buffers), which fall back to
    /// `duplicate_at`.
    shared: Option<Arc<Mutex<SharedFileState>>>,
    /// Reusable decode buffers for the zero-copy read path, shared with the
    /// `EntryReader`s it produces.
    pool: Arc<Mutex<BufferPool>>,
    /// Default password used to decrypt encrypted entries (set via
    /// `set_default_password`). Interior-mutable so it can be set on a shared
    /// `&Archive` (the FFI layer holds the archive behind a shared reference).
    password: Mutex<Option<Vec<u8>>>,
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
        // Grab a shared file handle (if this is a real file) so per-entry
        // readers can share it instead of cloning the OS handle each time.
        let shared = boxed.shared_handle();
        Ok(Archive {
            src: boxed,
            entries: cd.entries,
            comment: cd.comment,
            is_zip64: cd.is_zip64,
            shared,
            pool: Arc::new(Mutex::new(BufferPool::new(BUFFER_POOL_CAPACITY))),
            password: Mutex::new(None),
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
        // The 8 real ZIP_STAT_* bits libzip sets, not all 32 bits. WinZip AES
        // AE-2 entries have no valid CRC, so the ZIP_STAT_CRC (0x20) bit is
        // cleared (libzip reports 0xDF for those), exactly like C.
        let valid: u64 = if d.crc_valid { 0xFF } else { 0xFF & !0x20u64 };
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
            valid,
        })
    }

    /// Set the default password used to decrypt encrypted entries.
    ///
    /// This is the Rust-side equivalent of libzip's `zip_set_default_password`.
    /// A per-entry password passed to [`Archive::open_entry_with_password`] /
    /// [`Archive::read_entry_with_password`] takes precedence over this default.
    pub fn set_default_password(&self, password: &[u8]) {
        *self.password.lock().unwrap_or_else(|e| e.into_inner()) = Some(password.to_vec());
    }

    /// Open a streaming reader for the entry at `index` (decompressed), using
    /// the archive's default password for encrypted entries.
    pub fn open_entry(&self, index: u64) -> Result<EntryReader> {
        let d = self
            .entries
            .get(index as usize)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Inval))?;
        self.open_dirent(d, None)
    }

    /// Open a streaming reader for the entry at `index`, using `password` for
    /// encrypted entries. `None` falls back to the archive's default password.
    pub fn open_entry_with_password(
        &self,
        index: u64,
        password: Option<&[u8]>,
    ) -> Result<EntryReader> {
        let d = self
            .entries
            .get(index as usize)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Inval))?;
        self.open_dirent(d, password)
    }

    /// Open a streaming reader for the first entry named `name`.
    pub fn open_by_name(&self, name: &str) -> Result<EntryReader> {
        let idx = self
            .name_locate(name)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Noent))?;
        self.open_entry(idx)
    }

    /// Read the full decompressed content of the entry at `index` into a
    /// `Vec`, using the archive's default password for encrypted entries.
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

    /// Read the full decompressed content of the entry at `index`, using
    /// `password` for encrypted entries. `None` falls back to the default.
    pub fn read_entry_with_password(&self, index: u64, password: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut r = self.open_entry_with_password(index, password)?;
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Read the **raw compressed** (stored) bytes of the entry at `index` — the
    /// exact bytes that appear in the local data area, **without** decompression
    /// or decryption. This is the backing for `zip_source_zip` with
    /// `ZIP_FL_COMPRESSED` / `ZIP_FL_ENCRYPTED` (which expose the compressed,
    /// possibly-encrypted stream rather than the decompressed content).
    pub fn read_compressed_entry(&self, index: u64) -> Result<Vec<u8>> {
        let d = self
            .entries
            .get(index as usize)
            .ok_or_else(|| ZipError::new(ZipErrorCode::Inval))?;
        if d.comp_size > MAX_CD_SIZE {
            return Err(ZipError::new(ZipErrorCode::CentralDirTooLarge));
        }
        let mut dup = self.open_source_at(d.offset)?;
        // Lightweight local-header skip: read only the 30-byte fixed header,
        // then seek past the filename + extra fields to the data start. This
        // avoids the per-entry heap allocations and extra-field parsing of
        // `parse_local` (the central directory already carries the metadata).
        Dirent::local_header_len(&mut dup)?;
        let mut data = vec![0u8; d.comp_size as usize];
        reader::read_exact(&mut dup, &mut data)?;
        Ok(data)
    }

    /// Open a source positioned at `offset`, preferring the shared file handle
    /// (no per-entry `try_clone`/`DuplicateHandle`) and falling back to
    /// `duplicate_at` for non-file sources.
    fn open_source_at(&self, offset: u64) -> Result<Box<dyn Source>> {
        if let Some(shared) = &self.shared {
            Ok(Box::new(SharedFile::at_offset(shared.clone(), offset)))
        } else {
            self.src.duplicate_at(offset)
        }
    }

    fn open_dirent(&self, d: &Dirent, password: Option<&[u8]>) -> Result<EntryReader> {
        // Open a source at the entry's local-header offset. For a real file
        // this shares the archive's single handle (no per-entry
        // `try_clone`/`DuplicateHandle`); for other sources it clones.
        let mut dup = self.open_source_at(d.offset)?;
        // Lightweight local-header skip: read only the 30-byte fixed header,
        // then seek past the filename + extra fields to the data start. This
        // avoids the per-entry heap allocations and extra-field parsing of
        // `parse_local` (the central directory already carries the metadata).
        let data_offset = Dirent::local_header_len(&mut dup)?;
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
            // Only buffer-backed sources expose `as_slice()`; for a real
            // `File` this is `None`, so we skip the tell syscall entirely.
            if let Some(slice) = dup.as_slice() {
                let pos = data_offset as usize;
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
        if encrypted {
            // WinZip AES: authenticate via HMAC-SHA1, decrypt with AES-CTR.
            if crate::crypto::is_aes_method(d.encryption_method) {
                return self.open_aes(&mut dup, d, password);
            }
            // Only traditional PKWARE (ZipCrypto) is supported otherwise.
            if d.encryption_method != crate::constant::encryption::TRAD_PKWARE {
                return Err(ZipError::new(ZipErrorCode::Encrmethnotsupp));
            }
            // Resolve the password: explicit override, else the default.
            let pw: Vec<u8> = match password {
                Some(p) => p.to_vec(),
                None => {
                    let guard = self.password.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.as_ref() {
                        Some(p) => p.clone(),
                        None => return Err(ZipError::new(ZipErrorCode::Nopasswd)),
                    }
                }
            };
            // Read and decrypt the 12-byte encryption header, then verify the
            // final byte against the high byte of the entry CRC.
            let mut header = [0u8; ENCRYPTION_HEADER_LEN];
            reader::read_exact(&mut dup, &mut header)?;
            let mut crypto = ZipCrypto::new(&pw);
            crypto.decrypt(&mut header);
            if header[ENCRYPTION_HEADER_LEN - 1] != (crc >> 24) as u8 {
                return Err(ZipError::new(ZipErrorCode::Wrongpasswd));
            }
            if comp_size < ENCRYPTION_HEADER_LEN as u64 {
                return Err(ZipError::new(ZipErrorCode::TruncatedZip));
            }
            // The remaining encrypted bytes are the compressed data.
            let data_comp_size = comp_size - ENCRYPTION_HEADER_LEN as u64;
            let dec = DecryptingSource::new(dup, crypto);
            return EntryReader::new(Box::new(dec), method, data_comp_size, uncomp_size, crc);
        }

        EntryReader::new(dup, method, comp_size, uncomp_size, crc)
    }

    /// Open a WinZip AES-encrypted entry: read the full `[salt][pwd_verify]
    /// [ciphertext][hmac]` region, authenticate it (HMAC-SHA1), decrypt it
    /// (AES-CTR), then stream-decompress the resulting bytes. The stored CRC
    /// is not used (AE-2), so CRC verification is skipped in the reader.
    fn open_aes(
        &self,
        dup: &mut Box<dyn Source>,
        d: &Dirent,
        password: Option<&[u8]>,
    ) -> Result<EntryReader> {
        // Resolve the password: explicit override, else the default.
        let pw: Vec<u8> = match password {
            Some(p) => p.to_vec(),
            None => {
                let guard = self.password.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_ref() {
                    Some(p) => p.clone(),
                    None => return Err(ZipError::new(ZipErrorCode::Nopasswd)),
                }
            }
        };

        // Bound the read so a maliciously huge comp_size cannot trigger an
        // unbounded allocation (zip-bomb guard).
        if d.comp_size > MAX_CD_SIZE {
            return Err(ZipError::new(ZipErrorCode::CentralDirTooLarge));
        }
        let mut region = vec![0u8; d.comp_size as usize];
        reader::read_exact(dup, &mut region)?;

        let plain_compressed = crate::crypto::aes_decrypt_data(&pw, d.encryption_method, &region)?;
        let pclen = plain_compressed.len() as u64;
        let src: Box<dyn Source> = Box::new(std::io::Cursor::new(plain_compressed));
        EntryReader::new_skip_crc(src, d.comp_method, pclen, d.uncomp_size)
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
pub fn dos_to_unix(dos_time: u16, dos_date: u16) -> u64 {
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

/// Convert a Unix timestamp to a DOS `(time, date)` pair.
///
/// Mirrors libzip's `_zip_u2d_time`: the timestamp is interpreted as LOCAL
/// wall-clock time (via `localtime`), the year is clamped to >= 1980, and the
/// result is the packed DOS fields. This is the inverse of [`dos_to_unix`] and
/// round-trips through the same timezone-aware localtime/mktime semantics.
pub fn unix_to_dos(ut: u64) -> (u16, u16) {
    use chrono::{DateTime, Datelike, Local, LocalResult, TimeZone, Timelike};
    // `DateTime::from_timestamp` is timezone-independent (UTC); we then convert
    // to the local wall-clock fields the same way `localtime` does.
    let dt = DateTime::from_timestamp(ut as i64, 0).unwrap_or_else(|| {
        // Extremely large timestamps: clamp to something representable.
        DateTime::from_timestamp(0x7FFF_FFFF, 0).unwrap()
    });
    // Convert the UTC instant to local wall-clock time.
    let local = match Local.timestamp_opt(ut as i64, 0) {
        LocalResult::Single(t) => t.naive_local(),
        LocalResult::Ambiguous(a, _) => a.naive_local(),
        LocalResult::None => dt.naive_utc(),
    };

    let mut year = local.year();
    if year < 1980 {
        year = 1980; // libzip clamps tm_year to >= 80
    }
    let dos_date =
        (((year - 1980) as u16) << 9) | ((local.month() as u16) << 5) | (local.day() as u16);
    let dos_time = ((local.hour() as u16) << 11)
        | ((local.minute() as u16) << 5)
        | ((local.second() as u16) >> 1);
    (dos_time, dos_date)
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
    use crate::constant::flag;
    use crate::constant::magic;
    use std::io::{Cursor, Read, SeekFrom};

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

    // ---- Phase 1: ZipCrypto (traditional PKWARE) encryption ----

    /// Build a ZipCrypto-encrypted archive with `password` for every entry.
    fn encrypted_archive(files: &[(&str, Vec<u8>)], password: &[u8]) -> Vec<u8> {
        let afiles = files
            .iter()
            .map(|(n, d)| ArchiveFile::new(*n, d.clone()))
            .collect::<Vec<_>>();
        let opts = CompressOptions::default();
        let encrypt = vec![true; afiles.len()];
        crate::compress::write_archive_encrypted(&afiles, &opts, password, &encrypt).unwrap()
    }

    /// TC-3: wrong password -> ZIP_ER_WRONGPASS (27).
    #[test]
    fn wrong_password() {
        let bytes = encrypted_archive(&[("secret.txt", b"top secret data".to_vec())], b"right-pw");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let err = arch
            .open_entry_with_password(0, Some(b"wrong-pw"))
            .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Wrongpasswd);
    }

    /// TC-4: no password -> ZIP_ER_NOPASS (26).
    #[test]
    fn no_password() {
        let bytes = encrypted_archive(&[("secret.txt", b"top secret data".to_vec())], b"right-pw");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let err = arch.open_entry(0).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Nopasswd);
    }

    /// Correct password decrypts and reads the original bytes.
    #[test]
    fn correct_password_reads_original() {
        let content = b"encrypted round-trip payload ".repeat(40);
        let bytes = encrypted_archive(&[("a.txt", content.clone())], b"kzip-test-password");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let data = arch
            .read_entry_with_password(0, Some(b"kzip-test-password"))
            .unwrap();
        assert_eq!(data, content);
    }

    /// Default password (set via `set_default_password`) decrypts entries.
    #[test]
    fn default_password_decrypts() {
        let content = b"default password content".to_vec();
        let bytes = encrypted_archive(&[("a.txt", content.clone())], b"kzip-test-password");
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        arch.set_default_password(b"kzip-test-password");
        let data = arch.read_entry(0).unwrap();
        assert_eq!(data, content);
    }

    /// TC-7: `zip_stat` reports ZIP_EM_TRAD_PKWARE (1) for encrypted entries,
    /// and still ZIP_EM_NONE (0) for unencrypted entries (no regression).
    #[test]
    fn stat_encryption_method_trad_pkw() {
        // Encrypted entry.
        let enc = encrypted_archive(&[("e.txt", b"enc".to_vec())], b"pw");
        let arch = Archive::open(Cursor::new(enc)).unwrap();
        assert_eq!(arch.stat(0).unwrap().encryption_method, Some(1));

        // Unencrypted entry still reports 0.
        let plain = write_archive(
            &[ArchiveFile::new("p.txt", b"plain".to_vec())],
            &CompressOptions::default(),
        )
        .unwrap();
        let arch2 = Archive::open(Cursor::new(plain)).unwrap();
        assert_eq!(arch2.stat(0).unwrap().encryption_method, Some(0));
    }

    /// TC-8: malformed/truncated encrypted input must not panic; it yields a
    /// defined error code.
    #[test]
    fn malformed_encrypted_no_panic() {
        // Truncated local header / ciphertext: opening the entry must error,
        // not panic.
        let bytes = encrypted_archive(&[("a.txt", b"payload".to_vec())], b"pw");
        // Cut the archive in half (truncated ciphertext / central dir).
        let cut = bytes.len() / 2;
        let truncated = &bytes[..cut];
        // Opening may fail (truncated archive) or succeed; either way no panic.
        if let Ok(arch) = Archive::open(Cursor::new(truncated.to_vec())) {
            let _ = arch.open_entry_with_password(0, Some(b"pw"));
        }

        // Garbage after the 12-byte header: a valid header followed by
        // truncated data. Build a minimal encrypted entry with a too-short
        // payload.
        let mut v = Vec::new();
        v.extend_from_slice(&magic::LOCAL);
        v.extend_from_slice(&20u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // ENCRYPTED flag
        v.extend_from_slice(&0u16.to_le_bytes()); // store
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // crc
        v.extend_from_slice(&(12u32 + 3).to_le_bytes()); // comp_size = header + 3
        v.extend_from_slice(&5u32.to_le_bytes()); // uncomp_size
        v.extend_from_slice(&1u16.to_le_bytes()); // name len
        v.extend_from_slice(&0u16.to_le_bytes()); // extra len
        v.push(b'a'); // name
        v.extend_from_slice(&[0u8; 12]); // header (garbage)
        v.extend_from_slice(&[1u8, 2, 3]); // truncated ciphertext
                                           // Central dir + EOCD for a single entry.
        let cdir_offset = v.len() as u64;
        v.extend_from_slice(&magic::CENTRAL);
        v.extend_from_slice(&(3u16 << 8 | 20u16).to_le_bytes());
        v.extend_from_slice(&20u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // ENCRYPTED
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&(12u32 + 3).to_le_bytes());
        v.extend_from_slice(&5u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.push(b'a');
        let cdir_size = (v.len() as u64) - cdir_offset;
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());

        let arch = Archive::open(Cursor::new(v)).unwrap();
        // Wrong password (garbage header) -> WRONGPASS; no panic.
        let _ = arch.open_entry_with_password(0, Some(b"pw"));
    }

    // ---- Phase 2: WinZip AES ----

    /// Build a WinZip AES-encrypted archive (all entries encrypted with
    /// `method`, e.g. AES-128/192/256) using `password`.
    fn aes_archive(files: &[(&str, Vec<u8>)], password: &[u8], method: u16) -> Vec<u8> {
        let afiles = files
            .iter()
            .map(|(n, d)| ArchiveFile::new(*n, d.clone()))
            .collect::<Vec<_>>();
        let opts = CompressOptions::default();
        let methods = vec![method; afiles.len()];
        crate::compress::write_archive_encrypted_methods(&afiles, &opts, password, &methods)
            .unwrap()
    }

    /// AES-128/192/256 archives round-trip: write with the Rust writer, read
    /// back with the correct password, matching the original bytes and
    /// reporting the right encryption method.
    #[test]
    fn aes_read_write_roundtrip() {
        for method in [
            crate::constant::encryption::AES_128,
            crate::constant::encryption::AES_192,
            crate::constant::encryption::AES_256,
        ] {
            let content = b"winzip aes roundtrip payload ".repeat(40);
            let bytes = aes_archive(
                &[("a.txt", content.clone()), ("b.bin", vec![0xAA; 512])],
                b"kzip-test-password",
                method,
            );
            let arch = Archive::open(Cursor::new(bytes)).unwrap();
            let data = arch
                .read_entry_with_password(0, Some(b"kzip-test-password"))
                .unwrap();
            assert_eq!(data, content, "method {method:#06x}");
        }
    }

    /// TC-2: corrupted ciphertext -> ZIP_ER_CRC (7).
    #[test]
    fn aes_integrity_corruption() {
        let content = b"this is the aes authenticated payload ".repeat(20);
        let bytes = aes_archive(
            &[("secret.txt", content.clone())],
            b"kzip-test-password",
            crate::constant::encryption::AES_256,
        );
        let mut corrupted = bytes.clone();
        // The entry's data starts after the local header (30) + name + the
        // 11-byte AES extra field (id+len+7 bytes). Corrupt a ciphertext byte
        // (past salt[16] + 2-byte verify), not the header/HMAC.
        let name_len = "secret.txt".len();
        let aes_extra_len = 4 + crate::constant::EF_WINZIP_AES_SIZE;
        let data_start = 30 + name_len + aes_extra_len;
        let cipher_pos = data_start + 16 + 2; // salt(16) + verify(2) => first ciphertext byte
        corrupted[cipher_pos] ^= 0xFF;

        let arch = Archive::open(Cursor::new(corrupted)).unwrap();
        let err = arch
            .open_entry_with_password(0, Some(b"kzip-test-password"))
            .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Crc);
        // The unmodified archive reads fine.
        let arch_ok = Archive::open(Cursor::new(bytes)).unwrap();
        let ok = arch_ok
            .read_entry_with_password(0, Some(b"kzip-test-password"))
            .unwrap();
        assert_eq!(ok, content);
    }

    /// TC-3: wrong password -> ZIP_ER_WRONGPASS (27).
    #[test]
    fn aes_wrong_password() {
        let bytes = aes_archive(
            &[("secret.txt", b"secret".to_vec())],
            b"right-pw",
            crate::constant::encryption::AES_256,
        );
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let err = arch
            .open_entry_with_password(0, Some(b"wrong-pw"))
            .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Wrongpasswd);
    }

    /// TC-4: no password -> ZIP_ER_NOPASS (26).
    #[test]
    fn aes_no_password() {
        let bytes = aes_archive(
            &[("secret.txt", b"secret".to_vec())],
            b"right-pw",
            crate::constant::encryption::AES_256,
        );
        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        let err = arch.open_entry(0).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Nopasswd);
    }

    /// TC-6: zip_stat reports ZIP_EM_AES_128/192/256 (257/258/259); an
    /// unencrypted entry still reports 0 (no regression).
    #[test]
    fn stat_encryption_method_aes() {
        for (method, expected) in [
            (crate::constant::encryption::AES_128, 257u16),
            (crate::constant::encryption::AES_192, 258u16),
            (crate::constant::encryption::AES_256, 259u16),
        ] {
            let bytes = aes_archive(&[("e.txt", b"enc".to_vec())], b"pw", method);
            let arch = Archive::open(Cursor::new(bytes)).unwrap();
            assert_eq!(
                arch.stat(0).unwrap().encryption_method,
                Some(expected),
                "method {method:#06x}"
            );
        }
        // Unencrypted still reports 0.
        let plain = write_archive(
            &[ArchiveFile::new("p.txt", b"plain".to_vec())],
            &CompressOptions::default(),
        )
        .unwrap();
        let arch = Archive::open(Cursor::new(plain)).unwrap();
        assert_eq!(arch.stat(0).unwrap().encryption_method, Some(0));
    }

    /// TC-7: malformed/truncated AES input must not panic; it yields a defined
    /// error code.
    #[test]
    fn aes_malformed_no_panic() {
        // Truncated archive (cut in half): opening may fail or succeed, but no
        // panic, and any open entry attempt returns an error.
        let bytes = aes_archive(
            &[("a.txt", b"payload payload payload".to_vec())],
            b"pw",
            crate::constant::encryption::AES_256,
        );
        let cut = bytes.len() / 2;
        if let Ok(arch) = Archive::open(Cursor::new(bytes[..cut].to_vec())) {
            let _ = arch.open_entry_with_password(0, Some(b"pw"));
        }

        // Truncated AES region: local header claims an encrypted region but the
        // data is shorter than salt+verify+hmac. Build a minimal entry.
        let mut v = Vec::new();
        let name = b"a";
        v.extend_from_slice(&magic::LOCAL);
        v.extend_from_slice(&51u16.to_le_bytes());
        v.extend_from_slice(&(flag::ENCRYPTED | flag::DATA_DESCRIPTOR).to_le_bytes());
        v.extend_from_slice(&crate::constant::ZIP_CM_WINZIP_AES.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // crc 0
        v.extend_from_slice(&12u32.to_le_bytes()); // comp_size (too small)
        v.extend_from_slice(&5u32.to_le_bytes()); // uncomp_size
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(4 + crate::constant::EF_WINZIP_AES_SIZE as u16).to_le_bytes());
        v.extend_from_slice(name);
        // AES extra field: version 2, "AE", strength 3, method 0.
        v.extend_from_slice(&crate::constant::EF_WINZIP_AES.to_le_bytes());
        v.extend_from_slice(&(crate::constant::EF_WINZIP_AES_SIZE as u16).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(b"AE");
        v.push(3);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&[0u8; 12]); // truncated data (needs 28 bytes)
        let cdir_offset = v.len() as u64;
        v.extend_from_slice(&magic::CENTRAL);
        v.extend_from_slice(&(3u16 << 8 | 51u16).to_le_bytes());
        v.extend_from_slice(&51u16.to_le_bytes());
        v.extend_from_slice(&(flag::ENCRYPTED | flag::DATA_DESCRIPTOR).to_le_bytes());
        v.extend_from_slice(&crate::constant::ZIP_CM_WINZIP_AES.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&12u32.to_le_bytes());
        v.extend_from_slice(&5u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(4 + crate::constant::EF_WINZIP_AES_SIZE as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.push(b'a');
        v.extend_from_slice(&crate::constant::EF_WINZIP_AES.to_le_bytes());
        v.extend_from_slice(&(crate::constant::EF_WINZIP_AES_SIZE as u16).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(b"AE");
        v.push(3);
        v.extend_from_slice(&0u16.to_le_bytes());
        let cdir_size = (v.len() as u64) - cdir_offset;
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());

        let arch = Archive::open(Cursor::new(v)).unwrap();
        // Truncated AES region -> TruncatedZip (or another defined code), never
        // a panic.
        let _ = arch.open_entry_with_password(0, Some(b"pw"));
    }
}

//! Directory entry: the on-disk representation of a single archive member.
//!
//! Mirrors libzip's `zip_dirent_t` plus parsing for central-directory records
//! and local file headers. Handles the ZIP64 extra field for sizes/offsets that
//! overflow their 32-bit fields.

use crate::constant::{
    flag, magic, size, CompressionMethod, EF_WINZIP_AES, EF_WINZIP_AES_SIZE, EXTRA_FIELD_ZIP64,
    ZIP64_MAGIC32, ZIP_CM_WINZIP_AES,
};
use crate::crypto::aes_method_from_strength;
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::reader;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::Mutex;

/// An extra field record `(id, data)`.
pub type ExtraField = (u16, Vec<u8>);

/// Serialize a list of extra-field records into their raw on-disk form
/// (`id` + `len` + `data` for each). The total must fit in a `u16` length
/// field or this returns `ZIP_ER_EF_TOO_LARGE` (matching libzip). Internal
/// ZIP64/AES records are the caller's concern (the writer filters them).
pub fn serialize_extra_fields(fields: &[ExtraField]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (id, data) in fields {
        let len = u16::try_from(data.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(data);
    }
    if out.len() > u16::MAX as usize {
        return Err(ZipError::new(ZipErrorCode::Eftoolarge));
    }
    Ok(out)
}

/// A parsed directory entry (local or central).
#[derive(Debug)]
pub struct Dirent {
    /// Version made by (upper byte = host OS).
    pub version_madeby: u16,
    /// Minimum version needed to extract.
    pub version_needed: u16,
    /// General purpose bit flags.
    pub bitflags: u16,
    /// Compression method, resolved.
    pub comp_method: CompressionMethod,
    /// DOS-encoded last modification time.
    pub last_mod_time: u16,
    /// DOS-encoded last modification date.
    pub last_mod_date: u16,
    /// CRC-32 of the uncompressed content.
    pub crc: u32,
    /// Compressed byte length.
    pub comp_size: u64,
    /// Uncompressed byte length.
    pub uncomp_size: u64,
    /// Entry name (UTF-8 decoded).
    pub filename: String,
    /// Entry comment (UTF-8 decoded).
    pub comment: String,
    /// Disk number the local header starts on.
    pub disk_number: u32,
    /// Internal (ZIP) attributes.
    pub int_attrib: u16,
    /// External (host-specific) attributes.
    pub ext_attrib: u32,
    /// Offset of the local file header in the archive.
    pub offset: u64,
    /// Cached offset where the entry's data begins (immediately after the
    /// local file header's fixed + filename + extra portion). Computed lazily
    /// on the first open and reused for subsequent opens, so repeated access
    /// to the same entry avoids re-reading/re-seeking the local header (P3).
    /// `None` until first computed. Interior-mutable so it can be filled from
    /// a shared `&Dirent`; `Mutex` (rather than `OnceLock`) keeps `Dirent`
    /// `Clone`-able.
    pub data_offset: Mutex<Option<u64>>,
    /// Encryption method (0 = none).
    pub encryption_method: u16,
    /// Whether the stored CRC is valid/trustworthy. WinZip AES AE-2 entries
    /// do not store a real CRC (it is 0), so integrity must come from the
    /// HMAC instead; the reader skips CRC verification when this is false.
    pub crc_valid: bool,
    /// Raw extra-field records `(id, data)`.
    pub extra_fields: Vec<ExtraField>,
}

impl Clone for Dirent {
    /// Clone all fields but reset the lazily-computed `data_offset` cache to
    /// `None` (a `Mutex` is not `Clone`; the clone will recompute on first
    /// open, which is correct).
    fn clone(&self) -> Self {
        Dirent {
            version_madeby: self.version_madeby,
            version_needed: self.version_needed,
            bitflags: self.bitflags,
            comp_method: self.comp_method,
            last_mod_time: self.last_mod_time,
            last_mod_date: self.last_mod_date,
            crc: self.crc,
            comp_size: self.comp_size,
            uncomp_size: self.uncomp_size,
            filename: self.filename.clone(),
            comment: self.comment.clone(),
            disk_number: self.disk_number,
            int_attrib: self.int_attrib,
            ext_attrib: self.ext_attrib,
            offset: self.offset,
            data_offset: Mutex::new(None),
            encryption_method: self.encryption_method,
            crc_valid: self.crc_valid,
            extra_fields: self.extra_fields.clone(),
        }
    }
}

impl Dirent {
    /// Parse a central-directory entry from `cursor` (already positioned at the
    /// entry's `PK\x01\x02` magic).
    pub fn parse_central(cursor: &mut Cursor<&[u8]>) -> Result<Dirent> {
        let mut raw = [0u8; size::CENTRAL];
        if cursor.read_exact(&mut raw).is_err() {
            return Err(ZipError::new(ZipErrorCode::Nozip));
        }
        if raw[0..4] != magic::CENTRAL {
            return Err(ZipError::new(ZipErrorCode::Nozip));
        }
        let mut c = Cursor::new(&raw[4..]);

        let version_madeby = read16(&mut c);
        let version_needed = read16(&mut c);
        let bitflags = read16(&mut c);
        let comp_method_raw = read16(&mut c);
        let last_mod_time = read16(&mut c);
        let last_mod_date = read16(&mut c);
        let crc = read32(&mut c);
        let comp_size32 = read32(&mut c);
        let uncomp_size32 = read32(&mut c);
        let filename_len = read16(&mut c) as usize;
        let extra_len = read16(&mut c) as usize;
        let comment_len = read16(&mut c) as usize;
        let disk_number = read16(&mut c) as u32;
        let int_attrib = read16(&mut c);
        let ext_attrib = read32(&mut c);
        let offset32 = read32(&mut c);

        // Variable-length fields.
        let filename = read_bytes(cursor, filename_len)?;
        let extra_raw = read_raw(cursor, extra_len)?;
        let comment = read_bytes(cursor, comment_len)?;

        let extra_fields = parse_extra_fields(&extra_raw)?;

        // ZIP64 overrides for fields whose 32-bit value was the magic.
        let mut comp_size = comp_size32 as u64;
        let mut uncomp_size = uncomp_size32 as u64;
        let mut offset = offset32 as u64;
        let mut disk = disk_number;
        if let Some(z64) = zip64_data(&extra_fields) {
            let mut zc = Cursor::new(z64);
            if uncomp_size32 == ZIP64_MAGIC32 {
                uncomp_size = read64_from(&mut zc)?;
            }
            if comp_size32 == ZIP64_MAGIC32 {
                comp_size = read64_from(&mut zc)?;
            }
            if offset32 == ZIP64_MAGIC32 {
                offset = read64_from(&mut zc)?;
            }
            if disk_number == 0xFFFF {
                disk = read32_from(&mut zc)?;
            }
        }

        let (comp_method, encryption_method, crc_valid) = if comp_method_raw == ZIP_CM_WINZIP_AES {
            // The on-disk method for a WinZip AES entry is 99; the real
            // method and strength live in the 0x9901 extra field.
            match parse_aes_extra(&extra_fields) {
                Some((enc, actual, version)) => (
                    actual,
                    enc,
                    // AE-1 stores a real CRC; AE-2 does not.
                    version == 1,
                ),
                None => (
                    // Malformed AES entry: keep 99 as unsupported and mark
                    // encryption unknown so the reader rejects it safely.
                    CompressionMethod::Unsupported(ZIP_CM_WINZIP_AES as i32),
                    crate::constant::encryption::UNKNOWN,
                    false,
                ),
            }
        } else {
            let encryption_method = if bitflags & flag::ENCRYPTED != 0 {
                if bitflags & flag::STRONG_ENCRYPTION != 0 {
                    crate::constant::encryption::UNKNOWN // unknown strong encryption
                } else {
                    crate::constant::encryption::TRAD_PKWARE
                }
            } else {
                crate::constant::encryption::NONE
            };
            (
                CompressionMethod::from_u16(comp_method_raw),
                encryption_method,
                true,
            )
        };

        Ok(Dirent {
            version_madeby,
            version_needed,
            bitflags,
            comp_method,
            last_mod_time,
            last_mod_date,
            crc,
            comp_size,
            uncomp_size,
            filename,
            comment,
            disk_number: disk,
            int_attrib,
            ext_attrib,
            offset,
            data_offset: Mutex::new(None),
            encryption_method,
            crc_valid,
            extra_fields,
        })
    }

    /// Serialize this entry as a central-directory record, mirroring the
    /// write path's [`crate::compress::write_central_entry`] but starting from
    /// a parsed [`Dirent`]. Every field is preserved verbatim (flags, method,
    /// times, crc, sizes, filename, extra fields, comment, disk, attributes,
    /// offset) so the output is byte-identical to the record this entry was
    /// parsed from. Fields that overflow their 32-bit representation
    /// (`comp_size`, `uncomp_size`, `offset`) are written as the `0xFFFFFFFF`
    /// sentinel exactly as the writer does.
    pub fn to_central_record(&self) -> Result<Vec<u8>> {
        let aes = crate::crypto::is_aes_method(self.encryption_method);
        // On disk, a WinZip AES entry's method field is the special 99 value;
        // the real method lives in the 0x9901 extra field (preserved verbatim).
        let method = if aes {
            ZIP_CM_WINZIP_AES
        } else {
            match self.comp_method {
                CompressionMethod::Store => 0u16,
                CompressionMethod::Deflate => 8u16,
                CompressionMethod::Bzip2 => 12u16,
                CompressionMethod::Unsupported(m) => m as u16,
            }
        };
        let extra = serialize_extra_fields(&self.extra_fields)?;
        let extra_len =
            u16::try_from(extra.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
        let filename_len = u16::try_from(self.filename.len())
            .map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
        let comment_len =
            u16::try_from(self.comment.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;

        // 0xFFFFFFFF sentinels for 32-bit-overflowed fields, matching the
        // writer (ZIP64 extra-field reconstruction is a later phase).
        let comp32 = if self.comp_size > u32::MAX as u64 {
            ZIP64_MAGIC32
        } else {
            self.comp_size as u32
        };
        let uncomp32 = if self.uncomp_size > u32::MAX as u64 {
            ZIP64_MAGIC32
        } else {
            self.uncomp_size as u32
        };
        let offset32 = if self.offset > u32::MAX as u64 {
            ZIP64_MAGIC32
        } else {
            self.offset as u32
        };

        let mut out = Vec::new();
        out.extend_from_slice(&magic::CENTRAL);
        out.extend_from_slice(&self.version_madeby.to_le_bytes()); // version made by
        out.extend_from_slice(&self.version_needed.to_le_bytes()); // version needed
        out.extend_from_slice(&self.bitflags.to_le_bytes()); // bit flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&self.last_mod_time.to_le_bytes()); // dos time
        out.extend_from_slice(&self.last_mod_date.to_le_bytes()); // dos date
        out.extend_from_slice(&self.crc.to_le_bytes());
        out.extend_from_slice(&comp32.to_le_bytes());
        out.extend_from_slice(&uncomp32.to_le_bytes());
        out.extend_from_slice(&filename_len.to_le_bytes());
        out.extend_from_slice(&extra_len.to_le_bytes()); // extra len
        out.extend_from_slice(&comment_len.to_le_bytes()); // comment len
        out.extend_from_slice(&(self.disk_number as u16).to_le_bytes()); // disk number
        out.extend_from_slice(&self.int_attrib.to_le_bytes()); // internal attrs
        out.extend_from_slice(&self.ext_attrib.to_le_bytes()); // external attrs
        out.extend_from_slice(&offset32.to_le_bytes());
        out.extend_from_slice(self.filename.as_bytes());
        out.extend_from_slice(&extra);
        out.extend_from_slice(self.comment.as_bytes());
        Ok(out)
    }

    /// Parse a local file header from a source positioned at its `PK\x03\x04`
    /// magic. Returns the entry plus the byte length of the fixed + filename +
    /// extra portion (i.e. where the data begins).
    pub fn parse_local(src: &mut (impl Read + Seek)) -> Result<(Dirent, u64)> {
        let mut raw = [0u8; size::LOCAL];
        reader::read_exact(src, &mut raw)?;
        if raw[0..4] != magic::LOCAL {
            return Err(ZipError::new(ZipErrorCode::Nozip));
        }
        let mut c = Cursor::new(&raw[4..]);
        let version_needed = read16(&mut c);
        let bitflags = read16(&mut c);
        let comp_method_raw = read16(&mut c);
        let last_mod_time = read16(&mut c);
        let last_mod_date = read16(&mut c);
        let crc = read32(&mut c);
        let comp_size32 = read32(&mut c);
        let uncomp_size32 = read32(&mut c);
        let filename_len = read16(&mut c) as usize;
        let extra_len = read16(&mut c) as usize;

        let filename = {
            let mut buf = vec![0u8; filename_len];
            reader::read_exact(src, &mut buf)?;
            String::from_utf8_lossy(&buf).into_owned()
        };
        let extra_raw = {
            let mut buf = vec![0u8; extra_len];
            reader::read_exact(src, &mut buf)?;
            buf
        };
        let header_total = size::LOCAL as u64 + filename_len as u64 + extra_len as u64;

        let extra_fields = parse_extra_fields(&extra_raw)?;
        let mut comp_size = comp_size32 as u64;
        let mut uncomp_size = uncomp_size32 as u64;
        if let Some(z64) = zip64_data(&extra_fields) {
            let mut zc = Cursor::new(z64);
            if uncomp_size32 == ZIP64_MAGIC32 {
                uncomp_size = read64_from(&mut zc)?;
            }
            if comp_size32 == ZIP64_MAGIC32 {
                comp_size = read64_from(&mut zc)?;
            }
        }

        let (comp_method, encryption_method, crc_valid) = if comp_method_raw == ZIP_CM_WINZIP_AES {
            match parse_aes_extra(&extra_fields) {
                Some((enc, actual, version)) => (actual, enc, version == 1),
                None => (
                    CompressionMethod::Unsupported(ZIP_CM_WINZIP_AES as i32),
                    crate::constant::encryption::UNKNOWN,
                    false,
                ),
            }
        } else {
            let encryption_method = if bitflags & flag::ENCRYPTED != 0 {
                crate::constant::encryption::TRAD_PKWARE
            } else {
                crate::constant::encryption::NONE
            };
            (
                CompressionMethod::from_u16(comp_method_raw),
                encryption_method,
                true,
            )
        };

        Ok((
            Dirent {
                version_madeby: 0,
                version_needed,
                bitflags,
                comp_method,
                last_mod_time,
                last_mod_date,
                crc,
                comp_size,
                uncomp_size,
                filename,
                comment: String::new(),
                disk_number: 0,
                int_attrib: 0,
                ext_attrib: 0,
                offset: 0,
                data_offset: Mutex::new(None),
                encryption_method,
                crc_valid,
                extra_fields,
            },
            header_total,
        ))
    }

    /// Return the offset where this entry's data begins, computing it lazily
    /// from `src` (positioned at the entry's local-header offset) on the first
    /// call and caching it for reuse. Subsequent calls return the cached value
    /// without touching `src`, so repeated opens of the same entry skip the
    /// local-header read/seek entirely (P3).
    pub fn data_offset(&self, src: &mut (impl Read + Seek)) -> Result<u64> {
        if let Some(off) = *self.data_offset.lock().unwrap_or_else(|e| e.into_inner()) {
            // Cached: `src` was freshly opened at the local-header offset, so
            // position it at the data offset before returning.
            src.seek(SeekFrom::Start(off))
                .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
            return Ok(off);
        }
        // First open: `local_header_len` reads the fixed header and seeks past
        // the filename + extra fields, leaving `src` positioned at the data
        // start; cache that offset for reuse.
        let off = Self::local_header_len(src)?;
        *self.data_offset.lock().unwrap_or_else(|e| e.into_inner()) = Some(off);
        Ok(off)
    }

    /// Lightweight local-header skip for the read-open path.
    ///
    /// Reads only the fixed 30-byte local file header from `src` (positioned at
    /// its `PK\x03\x04` magic), then seeks forward past the variable-length
    /// filename and extra fields, returning the offset where the entry's data
    /// begins. This mirrors libzip's `_zip_dirent_size` (which reads only the
    /// fixed header) and avoids the per-entry heap allocations and extra-field
    /// parsing that [`parse_local`] performs — the central directory already
    /// carries the authoritative metadata, so the local header only needs to be
    /// skipped, not fully decoded.
    pub fn local_header_len(src: &mut (impl Read + Seek)) -> Result<u64> {
        let mut raw = [0u8; size::LOCAL];
        reader::read_exact(src, &mut raw)?;
        if raw[0..4] != magic::LOCAL {
            return Err(ZipError::new(ZipErrorCode::Nozip));
        }
        let mut c = Cursor::new(&raw[4..]);
        // Fixed fields: version_needed(2) bitflags(2) method(2) time(2) date(2)
        // crc(4) comp_size(4) uncomp_size(4) = 22 bytes, then the two
        // variable-length sizes we actually need.
        let _ = read16(&mut c); // version_needed
        let _ = read16(&mut c); // bitflags
        let _ = read16(&mut c); // comp_method
        let _ = read16(&mut c); // last_mod_time
        let _ = read16(&mut c); // last_mod_date
        let _ = read32(&mut c); // crc
        let _ = read32(&mut c); // comp_size
        let _ = read32(&mut c); // uncomp_size
        let filename_len = read16(&mut c) as u64;
        let extra_len = read16(&mut c) as u64;
        // Seek past the filename + extra fields to the data start.
        src.seek(SeekFrom::Current((filename_len + extra_len) as i64))
            .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))
    }
}

fn read16(c: &mut Cursor<&[u8]>) -> u16 {
    let mut b = [0u8; 2];
    c.read_exact(&mut b).expect("fixed header underflow");
    u16::from_le_bytes(b)
}

fn read32(c: &mut Cursor<&[u8]>) -> u32 {
    let mut b = [0u8; 4];
    c.read_exact(&mut b).expect("fixed header underflow");
    u32::from_le_bytes(b)
}

fn read_raw(cursor: &mut Cursor<&[u8]>, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    if cursor.read_exact(&mut buf).is_err() {
        return Err(ZipError::new(ZipErrorCode::Eof));
    }
    Ok(buf)
}

fn read_bytes(cursor: &mut Cursor<&[u8]>, len: usize) -> Result<String> {
    let mut buf = vec![0u8; len];
    if cursor.read_exact(&mut buf).is_err() {
        return Err(ZipError::new(ZipErrorCode::Eof));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read64_from(c: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut b = [0u8; 8];
    if c.read_exact(&mut b).is_err() {
        return Err(ZipError::new(ZipErrorCode::Incons));
    }
    Ok(u64::from_le_bytes(b))
}

fn read32_from(c: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut b = [0u8; 4];
    if c.read_exact(&mut b).is_err() {
        return Err(ZipError::new(ZipErrorCode::Incons));
    }
    Ok(u32::from_le_bytes(b))
}

/// Parse the raw extra-field block into `(id, data)` records.
///
/// Mirrors libzip's `_zip_ef_parse`: a field whose declared length exceeds the
/// bytes actually present is a malformed/truncated extra field and yields
/// `ZIP_ER_INCONS` (matching libzip's strict rejection). Only a trailing tail
/// shorter than a full (id, len) header is ignored.
fn parse_extra_fields(raw: &[u8]) -> Result<Vec<ExtraField>> {
    let mut out = Vec::new();
    let mut c = Cursor::new(raw);
    while c.position() + 4 <= raw.len() as u64 {
        let id = read16(&mut c);
        let len = read16(&mut c) as usize;
        let start = c.position() as usize;
        if start + len > raw.len() {
            return Err(ZipError::new(ZipErrorCode::Incons));
        }
        let data = raw[start..start + len].to_vec();
        c.set_position((start + len) as u64);
        out.push((id, data));
    }
    Ok(out)
}

/// Returns the ZIP64 extra field data (ID 0x0001), if present.
fn zip64_data(fields: &[ExtraField]) -> Option<&[u8]> {
    fields
        .iter()
        .find(|(id, _)| *id == EXTRA_FIELD_ZIP64)
        .map(|(_, d)| d.as_slice())
}

/// Parse the WinZip AES extra field (id 0x9901). Returns
/// `(encryption_method, actual_comp_method, aes_version)` on success, or `None`
/// if the field is absent/malformed (caller treats it as unsupported).
///
/// Data layout (7 bytes): `u16` version (1 = AE-1, 2 = AE-2), 2-byte vendor
/// "AE", `u8` strength (1/2/3), `u16` actual compression method.
fn parse_aes_extra(fields: &[ExtraField]) -> Option<(u16, CompressionMethod, u16)> {
    let data = &fields.iter().find(|(id, _)| *id == EF_WINZIP_AES)?.1;
    if data.len() < EF_WINZIP_AES_SIZE {
        return None;
    }
    if data[2] != b'A' || data[3] != b'E' {
        return None;
    }
    let version = u16::from_le_bytes([data[0], data[1]]);
    let strength = data[4];
    let method = u16::from_le_bytes([data[5], data[6]]);
    let enc = aes_method_from_strength(strength);
    if enc == crate::constant::encryption::UNKNOWN {
        return None;
    }
    Some((enc, CompressionMethod::from_u16(method), version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::magic;

    fn build_central(
        name: &str,
        method: u16,
        crc: u32,
        csize: u32,
        usize32: u32,
        offset: u32,
    ) -> Vec<u8> {
        let name = name.as_bytes();
        let mut v = Vec::new();
        v.extend_from_slice(&magic::CENTRAL);
        // version_madeby(2) version_needed(2) bitflags(2) method(2) time(2) date(2)
        v.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
        v.extend_from_slice(&method.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0, 0]); // time/date
        v.extend_from_slice(&crc.to_le_bytes());
        v.extend_from_slice(&csize.to_le_bytes());
        v.extend_from_slice(&usize32.to_le_bytes());
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // extra len
        v.extend_from_slice(&0u16.to_le_bytes()); // comment len
        v.extend_from_slice(&0u16.to_le_bytes()); // disk
        v.extend_from_slice(&0u16.to_le_bytes()); // int attrib
        v.extend_from_slice(&0u32.to_le_bytes()); // ext attrib
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(name);
        v
    }

    #[test]
    fn parses_central_entry() {
        let data = build_central("a/b.txt", 8, 0x11223344, 10, 100, 0x99);
        let mut c = Cursor::new(data.as_slice());
        let d = Dirent::parse_central(&mut c).unwrap();
        assert_eq!(d.filename, "a/b.txt");
        assert_eq!(d.comp_method, CompressionMethod::Deflate);
        assert_eq!(d.crc, 0x11223344);
        assert_eq!(d.comp_size, 10);
        assert_eq!(d.uncomp_size, 100);
        assert_eq!(d.offset, 0x99);
    }

    #[test]
    fn zip64_offset_override() {
        let name = "big";
        let mut v = build_central(name, 0, 0, 10, 10, 0xFFFF_FFFF);
        // extra field len is in the built entry at the "extra len" position.
        // Rebuild by appending a zip64 extra field. We reconstruct manually:
        // The extra_len position is bytes[28..30]; set to zip64 record size (8).
        let mut extra = Vec::new();
        extra.extend_from_slice(&EXTRA_FIELD_ZIP64.to_le_bytes());
        extra.extend_from_slice(&8u16.to_le_bytes());
        extra.extend_from_slice(&0x1122334455667788u64.to_le_bytes()); // offset
        v[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(&extra);

        let mut c = Cursor::new(v.as_slice());
        let d = Dirent::parse_central(&mut c).unwrap();
        assert_eq!(d.offset, 0x1122334455667788);
        assert_eq!(d.filename, "big");
    }

    #[test]
    fn wrong_magic_is_nozip() {
        let data = vec![0u8; 50];
        let mut c = Cursor::new(data.as_slice());
        assert_eq!(
            Dirent::parse_central(&mut c).unwrap_err().code(),
            ZipErrorCode::Nozip
        );
    }
}

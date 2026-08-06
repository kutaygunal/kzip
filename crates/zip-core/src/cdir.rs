//! Central directory: locating and reading the archive's central directory.
//!
//! Mirrors libzip's `_zip_find_central_dir` / `_zip_read_cdir`: scan the tail of
//! the archive for the EOCD record (tolerating trailing data), honor the ZIP64
//! EOCD, then read and validate the central directory entries.

use crate::constant::{magic, size, MAX_CD_BUFFER, MAX_CD_SIZE};
use crate::dirent::Dirent;
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::reader;
use crate::source::Source;
use std::io::{Read, Seek, SeekFrom};

/// Parsed central directory of an archive.
#[derive(Debug, Clone, Default)]
pub struct CentralDir {
    /// Directory entries, in central-directory order.
    pub entries: Vec<Dirent>,
    /// Archive (EOCD) comment.
    pub comment: String,
    /// Whether the central directory used the ZIP64 EOCD record.
    pub is_zip64: bool,
    /// Byte length of the central-directory region (all entries), as recorded
    /// by the EOCD (or ZIP64 EOCD when present).
    pub cdir_size: u64,
    /// Absolute offset of the central-directory region within the archive, as
    /// recorded by the EOCD (or ZIP64 EOCD when present).
    pub cdir_offset: u64,
}

impl CentralDir {
    /// Serialize the central directory: every per-entry record (via
    /// [`Dirent::to_central_record`]) followed by the EOCD record.
    ///
    /// The EOCD's `cdir_size` is the sum of the per-entry records and its
    /// `cdir_offset` is this archive's recorded [`CentralDir::cdir_offset`]; the
    /// entry count comes from `self.entries`. ZIP64 archives are not yet
    /// supported by the writer (added in a later phase) and return an
    /// "operation not supported" error.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        if self.is_zip64 {
            return Err(ZipError::new(ZipErrorCode::Opnotsupp));
        }
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(&e.to_central_record()?);
        }
        let cdir_size = out.len() as u64;
        write_eocd(
            self.entries.len() as u16,
            cdir_size,
            self.cdir_offset,
            self.comment.as_bytes(),
            &mut out,
        );
        Ok(out)
    }
}

/// Serialize an EOCD record into `out`. Shared with the compression write
/// path ([`crate::compress`]) so the serializer and the writer produce
/// identical EOCD records.
pub(crate) fn write_eocd(
    num_entries: u16,
    cdir_size: u64,
    cdir_offset: u64,
    comment: &[u8],
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&magic::EOCD);
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with cdir
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&(cdir_size as u32).to_le_bytes());
    out.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes()); // comment len
    out.extend_from_slice(comment);
}

/// Read the central directory from a seekable source.
pub fn read_central_dir(src: &mut Box<dyn Source>) -> Result<CentralDir> {
    let len = source_len(src)?;
    if len < size::EOCD as u64 {
        return Err(ZipError::new(ZipErrorCode::Nozip));
    }

    let buflen = std::cmp::min(len, MAX_CD_BUFFER as u64) as usize;
    let tail_start = len - buflen as u64;
    src.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0u8; buflen];
    reader::read_exact(src, &mut tail)?;

    // Candidate EOCD absolute offsets, from EOF backwards.
    let mut candidates = Vec::new();
    if buflen >= size::EOCD {
        for i in (0..=(buflen - size::EOCD)).rev() {
            if tail[i..i + 4] == magic::EOCD {
                candidates.push(tail_start + i as u64);
            }
        }
    }

    for abs in candidates {
        if let Some(cd) = try_parse_cdir(src, &tail, tail_start, abs)? {
            return Ok(cd);
        }
    }

    // No valid EOCD was found. Distinguish a truncated zip from a non-zip the
    // way libzip does (`_zip_find_central_dir`): if the archive begins with a
    // ZIP signature (`PK`) but lacks a parseable EOCD, it is a truncated zip
    // (ZIP_ER_TRUNCATED_ZIP); otherwise it is not a zip at all
    // (ZIP_ER_NOZIP).
    if source_starts_with_pk(src)? {
        return Err(ZipError::new(ZipErrorCode::TruncatedZip));
    }
    Err(ZipError::new(ZipErrorCode::Nozip))
}

/// Whether the source begins with the `PK` signature shared by all ZIP record
/// magics (local header, central directory, EOCD, ZIP64 EOCD).
fn source_starts_with_pk(src: &mut Box<dyn Source>) -> Result<bool> {
    let pos = src
        .stream_position()
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    src.seek(SeekFrom::Start(0))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    let mut head = [0u8; 2];
    let n = src
        .read(&mut head)
        .map_err(|e| ZipError::with_system(ZipErrorCode::Read, e))?;
    src.seek(SeekFrom::Start(pos))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    Ok(n == 2 && head == [0x50, 0x4B])
}

/// Attempt to parse the central directory for the EOCD record located at
/// absolute offset `abs`. Returns `Ok(None)` to keep searching for an earlier
/// EOCD candidate, or `Ok(Some(cd))` on a consistent parse.
fn try_parse_cdir(
    src: &mut Box<dyn Source>,
    tail: &[u8],
    tail_start: u64,
    abs: u64,
) -> Result<Option<CentralDir>> {
    let rel = (abs - tail_start) as usize;

    // EOCD fixed fields (after the 4-byte magic).
    if rel + size::EOCD > tail.len() {
        return Ok(None);
    }
    let e = &tail[rel..rel + size::EOCD];
    let this_disk = u16::from_le_bytes([e[4], e[5]]);
    let eocd_disk = u16::from_le_bytes([e[6], e[7]]);
    let disk_entries = u16::from_le_bytes([e[8], e[9]]);
    let num_entries = u16::from_le_bytes([e[10], e[11]]);
    let mut cdir_size = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
    let mut cdir_offset = u32::from_le_bytes([e[16], e[17], e[18], e[19]]) as u64;
    let comment_len = u16::from_le_bytes([e[20], e[21]]) as usize;

    // The comment must fit within the tail buffer.
    if rel + size::EOCD + comment_len > tail.len() {
        return Ok(None);
    }
    let comment = String::from_utf8_lossy(&tail[rel + size::EOCD..rel + size::EOCD + comment_len])
        .into_owned();

    // Multi-disk archives are unsupported.
    if this_disk != 0 || eocd_disk != 0 || disk_entries != num_entries {
        // For Phase 1 we treat multi-disk / mismatched EOCDs as "not found"
        // rather than erroring immediately.
        if this_disk != 0 || eocd_disk != 0 {
            return Ok(None);
        }
    }

    let mut is_zip64 = false;
    // ZIP64 EOCD locator sits directly before the EOCD.
    if rel >= size::EOCD64_LOCATOR
        && tail[rel - size::EOCD64_LOCATOR..rel - size::EOCD64_LOCATOR + 4] == magic::EOCD64_LOCATOR
    {
        is_zip64 = true;
        // Locator: disk(4) eocd64_offset(8) total_disks(4) = 16 bytes after magic.
        let loc = &tail[rel - size::EOCD64_LOCATOR + 4..rel - size::EOCD64_LOCATOR + 20];
        let eocd64_offset = u64::from_le_bytes([
            loc[4], loc[5], loc[6], loc[7], loc[8], loc[9], loc[10], loc[11],
        ]);
        // Read the ZIP64 EOCD record from its absolute offset.
        if let Some(z) = read_eocd64(src, eocd64_offset)? {
            cdir_size = z.cdir_size;
            cdir_offset = z.cdir_offset;
            // The 16-bit counts in the 32-bit EOCD are 0xFFFF when zip64 is used.
            if num_entries == 0xFFFF {
                // zip64 counts override; use z.num_entries.
                // We read entries from z.num_entries (u64).
                let entries = read_entries(src, cdir_offset, cdir_size, z.num_entries)?;
                return Ok(Some(CentralDir {
                    entries,
                    comment,
                    is_zip64,
                    cdir_size: z.cdir_size,
                    cdir_offset: z.cdir_offset,
                }));
            }
        }
    }

    if cdir_size == 0 && cdir_offset == 0 && num_entries == 0 {
        // Empty archive.
        return Ok(Some(CentralDir {
            entries: Vec::new(),
            comment,
            is_zip64,
            cdir_size: 0,
            cdir_offset: 0,
        }));
    }

    let entries = read_entries(src, cdir_offset, cdir_size, num_entries as u64)?;
    Ok(Some(CentralDir {
        entries,
        comment,
        is_zip64,
        cdir_size,
        cdir_offset,
    }))
}

/// ZIP64 EOCD fields.
struct Zip64Eocd {
    cdir_size: u64,
    cdir_offset: u64,
    num_entries: u64,
}

/// Read the ZIP64 EOCD record at `offset`. Returns `Ok(None)` if not parseable
/// (treat as "no zip64").
fn read_eocd64(src: &mut Box<dyn Source>, offset: u64) -> Result<Option<Zip64Eocd>> {
    if src.seek(SeekFrom::Start(offset)).is_err() {
        return Ok(None);
    }
    let mut raw = [0u8; size::EOCD64];
    if src.read_exact(&mut raw).is_err() {
        return Ok(None);
    }
    if raw[0..4] != magic::EOCD64 {
        return Ok(None);
    }
    // ZIP64 EOCD record layout (PK\x06\x06), offsets relative to the magic:
    //   [0:4]   magic
    //   [4:12]  size of the ZIP64 EOCD record (excluding these 12 bytes)
    //   [12:16] version made by (2) + version needed (2)
    //   [16:20] number of this disk
    //   [20:24] disk with the start of the central directory
    //   [24:32] entries on this disk
    //   [32:40] total entries in the archive
    //   [40:48] size of the central directory
    //   [48:56] offset of the central directory
    let cdir_size = u64::from_le_bytes(raw[40..48].try_into().unwrap());
    let cdir_offset = u64::from_le_bytes(raw[48..56].try_into().unwrap());
    let num_entries = u64::from_le_bytes(raw[32..40].try_into().unwrap());
    Ok(Some(Zip64Eocd {
        cdir_size,
        cdir_offset,
        num_entries,
    }))
}

/// Read `num_entries` central-directory entries starting at `offset` for
/// `cdir_size` bytes. Handles the InfoZIP `num_entries % 0x10000` truncation
/// trick by parsing as many entries as fit in the buffer.
fn read_entries(
    src: &mut Box<dyn Source>,
    offset: u64,
    cdir_size: u64,
    num_entries: u64,
) -> Result<Vec<Dirent>> {
    // Zip-bomb guard: reject an absurd central-directory size before
    // allocating, so an EOCD/ZIP64 record claiming e.g. `u32::MAX` (~4 GiB)
    // cannot trigger an unbounded allocation / OOM.
    if cdir_size > MAX_CD_SIZE {
        return Err(ZipError::new(ZipErrorCode::CentralDirTooLarge));
    }
    src.seek(SeekFrom::Start(offset))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    let mut buf = vec![0u8; cdir_size as usize];
    reader::read_exact(src, &mut buf).map_err(|e| match e.code() {
        ZipErrorCode::Eof => ZipError::new(ZipErrorCode::Incons),
        other => other.into(),
    })?;

    let mut cursor = std::io::Cursor::new(buf.as_slice());
    let mut entries = Vec::new();
    while (cursor.position() as usize) < buf.len() {
        match Dirent::parse_central(&mut cursor) {
            Ok(d) => entries.push(d),
            Err(_) => break,
        }
    }

    // Validate the count, allowing the InfoZIP 0x10000 wrap.
    let parsed = entries.len() as u64;
    let valid = parsed == num_entries || (parsed % 0x10000 == num_entries);
    if !valid {
        return Err(ZipError::new(ZipErrorCode::Incons));
    }
    Ok(entries)
}

fn source_len(src: &mut Box<dyn Source>) -> Result<u64> {
    let pos = src
        .stream_position()
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    let len = src
        .seek(SeekFrom::End(0))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    src.seek(SeekFrom::Start(pos))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{magic, size};
    use std::io::Cursor;

    /// Build a minimal single-file archive in memory and return its bytes.
    fn build_archive(filename: &str, content: &[u8]) -> Vec<u8> {
        let name = filename.as_bytes();
        // Local header.
        let mut v = Vec::new();
        v.extend_from_slice(&magic::LOCAL);
        v.extend_from_slice(&[20, 0, 0, 0]); // version_needed(2) bitflags(2)
        v.extend_from_slice(&0u16.to_le_bytes()); // method = store
        v.extend_from_slice(&[0u8; 4]); // time/date
        v.extend_from_slice(&0u32.to_le_bytes()); // crc (unused)
        v.extend_from_slice(&(content.len() as u32).to_le_bytes()); // comp
        v.extend_from_slice(&(content.len() as u32).to_le_bytes()); // uncomp
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // extra
        v.extend_from_slice(name);

        v.extend_from_slice(content);
        let cdir_offset = v.len() as u64;

        // Central directory entry.
        v.extend_from_slice(&magic::CENTRAL);
        v.extend_from_slice(&[20, 0, 20, 0, 0, 0]); // madeby needed flags
        v.extend_from_slice(&0u16.to_le_bytes()); // method
        v.extend_from_slice(&[0u8; 4]); // time/date
        v.extend_from_slice(&0u32.to_le_bytes()); // crc
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

        // EOCD.
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes()); // this_disk
        v.extend_from_slice(&0u16.to_le_bytes()); // eocd_disk
        v.extend_from_slice(&1u16.to_le_bytes()); // disk_entries
        v.extend_from_slice(&1u16.to_le_bytes()); // num_entries
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // comment
        v
    }

    #[test]
    fn reads_central_dir() {
        let bytes = build_archive("hello.txt", b"hello world");
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes));
        let cd = read_central_dir(&mut src).unwrap();
        assert_eq!(cd.entries.len(), 1);
        assert_eq!(cd.entries[0].filename, "hello.txt");
        assert_eq!(cd.entries[0].uncomp_size, 11);
    }

    #[test]
    fn not_a_zip_is_nozip() {
        let bytes = b"this is not a zip archive at all, just plain text data....".to_vec();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes));
        assert_eq!(
            read_central_dir(&mut src).unwrap_err().code(),
            ZipErrorCode::Nozip
        );
    }

    /// The ZIP64 EOCD field offsets must follow the PK\x06\x06 layout:
    /// cdir_size at [40:48], cdir_offset at [48:56], total entries at [32:40].
    /// This guards against the off-by-16 regression that made >65535-entry
    /// archives unreadable (ZIP_ER_INCONS).
    #[test]
    fn read_zip64_eocd_correct_field_offsets() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&magic::EOCD64); // [0:4]
        raw.extend_from_slice(&44u64.to_le_bytes()); // [4:12] record size
        raw.extend_from_slice(&0u32.to_le_bytes()); // [12:16] version made by + needed
        raw.extend_from_slice(&0u32.to_le_bytes()); // [16:20] number of this disk
        raw.extend_from_slice(&0u32.to_le_bytes()); // [20:24] disk with start of CD
        raw.extend_from_slice(&123u64.to_le_bytes()); // [24:32] entries on this disk
        raw.extend_from_slice(&70000u64.to_le_bytes()); // [32:40] total entries
        raw.extend_from_slice(&3920000u64.to_le_bytes()); // [40:48] central dir size
        raw.extend_from_slice(&3080000u64.to_le_bytes()); // [48:56] central dir offset

        let mut src: Box<dyn Source> = Box::new(Cursor::new(raw));
        let z = read_eocd64(&mut src, 0).unwrap().expect("parse zip64 eocd");
        assert_eq!(z.cdir_size, 3920000);
        assert_eq!(z.cdir_offset, 3080000);
        assert_eq!(z.num_entries, 70000);
    }

    /// A file that begins with a ZIP signature but has no parseable EOCD is a
    /// truncated archive (ZIP_ER_TRUNCATED_ZIP = 35), not a non-zip.
    #[test]
    fn truncated_pk_prefix_is_truncated_zip() {
        // First half of a valid archive: local header magic present, but no
        // central directory / EOCD. Must report 35, matching libzip.
        let local = [0x50, 0x4B, 0x03, 0x04];
        let mut bytes = local.to_vec();
        bytes.extend_from_slice(&[0u8; 4000]);
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes));
        assert_eq!(
            read_central_dir(&mut src).unwrap_err().code(),
            ZipErrorCode::TruncatedZip
        );
    }

    /// Build a small multi-entry archive (with per-entry extra fields and
    /// comments) and return `(full_bytes, cdir_offset, cdir_size)`.
    fn build_corpus_archive() -> (Vec<u8>, u64, u64) {
        // (name, method, crc, csize, usize, extra_raw, comment)
        let entries: Vec<(&str, u16, u32, u32, u32, &[u8], &str)> = vec![
            (
                "a/one.txt",
                8,
                0x11111111,
                20,
                100,
                &[0x01, 0x00, 0x03, 0x00, 0xde, 0xad, 0xbe],
                "entry comment",
            ),
            ("b/two.bin", 0, 0x22222222, 64, 64, &[], ""),
        ];
        let mut v = Vec::new();
        let mut offsets = Vec::new();
        // Local headers + data; record each entry's real offset.
        for (name, method, crc, csize, usize32, _extra, _comment) in &entries {
            offsets.push(v.len() as u32);
            let nameb = name.as_bytes();
            v.extend_from_slice(&magic::LOCAL);
            v.extend_from_slice(&[20, 0, 0, 0]); // version_needed + flags
            v.extend_from_slice(&method.to_le_bytes());
            v.extend_from_slice(&[0u8; 4]); // time/date
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&csize.to_le_bytes());
            v.extend_from_slice(&usize32.to_le_bytes());
            v.extend_from_slice(&(nameb.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // extra len
            v.extend_from_slice(nameb);
            v.extend_from_slice(&vec![0u8; *csize as usize]);
        }
        let cdir_offset = v.len() as u64;
        // Central directory entries (with extra + comment to exercise fidelity).
        for (idx, (name, method, crc, csize, usize32, extra, comment)) in entries.iter().enumerate()
        {
            let nameb = name.as_bytes();
            v.extend_from_slice(&magic::CENTRAL);
            v.extend_from_slice(&[20, 0, 20, 0, 0, 0]); // madeby needed flags
            v.extend_from_slice(&method.to_le_bytes());
            v.extend_from_slice(&[0u8; 4]); // time/date
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&csize.to_le_bytes());
            v.extend_from_slice(&usize32.to_le_bytes());
            v.extend_from_slice(&(nameb.len() as u16).to_le_bytes());
            v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            v.extend_from_slice(&(comment.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes()); // disk
            v.extend_from_slice(&0u16.to_le_bytes()); // int attrib
            v.extend_from_slice(&0u32.to_le_bytes()); // ext attrib
            v.extend_from_slice(&offsets[idx].to_le_bytes());
            v.extend_from_slice(nameb);
            v.extend_from_slice(extra);
            v.extend_from_slice(comment.as_bytes());
        }
        let cdir_size = (v.len() - cdir_offset as usize) as u64;
        // EOCD.
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes()); // this disk
        v.extend_from_slice(&0u16.to_le_bytes()); // disk with cdir
        v.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // disk entries
        v.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // num entries
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // comment len
        (v, cdir_offset, cdir_size)
    }

    /// Round-trip the central directory through `Dirent::to_central_record` +
    /// `CentralDir::serialize` and assert byte-identical to the original
    /// central-directory region (flags, extra fields, ordering all preserved).
    #[test]
    fn dirent_serialize_roundtrip() {
        let (bytes, cdir_offset, cdir_size) = build_corpus_archive();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes.clone()));
        let cd = read_central_dir(&mut src).unwrap();
        assert_eq!(cd.cdir_offset, cdir_offset);
        assert_eq!(cd.cdir_size, cdir_size);
        assert_eq!(cd.entries.len(), 2);

        let serialized = cd.serialize().unwrap();
        // Serializer emits the per-entry records followed by the EOCD; the
        // records region (first cdir_size bytes) must equal the original CD.
        assert_eq!(serialized.len(), cdir_size as usize + size::EOCD);
        let end = (cdir_offset + cdir_size) as usize;
        assert_eq!(
            &serialized[..cdir_size as usize],
            &bytes[cdir_offset as usize..end]
        );
    }

    /// `read_entries` must reject an absurd `cdir_size` (e.g. `u32::MAX` ≈
    /// 4 GiB) with `CentralDirTooLarge` *before* allocating, so a malicious
    /// EOCD cannot trigger an OOM.
    #[test]
    fn read_entries_rejects_absurd_cdir_size() {
        let mut src: Box<dyn Source> = Box::new(Cursor::new(vec![0u8; 16]));
        let err = read_entries(&mut src, 0, u32::MAX as u64, 1).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::CentralDirTooLarge);
    }

    /// End-to-end: an archive whose EOCD claims `cdir_size = u32::MAX` is
    /// rejected with `CentralDirTooLarge` without allocating gigabytes.
    #[test]
    fn eocd_claiming_huge_cdir_size_is_rejected() {
        // 4 bytes of junk + a 22-byte EOCD claiming cdir_size = u32::MAX.
        let mut bytes = vec![0u8; 4];
        bytes.extend_from_slice(&magic::EOCD);
        bytes.extend_from_slice(&0u16.to_le_bytes()); // this_disk
        bytes.extend_from_slice(&0u16.to_le_bytes()); // eocd_disk
        bytes.extend_from_slice(&1u16.to_le_bytes()); // disk_entries
        bytes.extend_from_slice(&1u16.to_le_bytes()); // num_entries
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // cdir_size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // cdir_offset
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment_len

        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes));
        assert_eq!(
            read_central_dir(&mut src).unwrap_err().code(),
            ZipErrorCode::CentralDirTooLarge
        );
    }
}

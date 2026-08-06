//! In-memory, byte-array true in-place modification of a ZIP archive.
//!
//! [`modify_archive`] parses the central directory of an existing archive held
//! entirely in a `&[u8]`, applies renames and deletions by original index, and
//! produces a new archive **without any file I/O and without recompressing any
//! stored data**. The intact local-header + compressed-data region is copied
//! verbatim; only the central directory and EOCD are re-serialized.
//!
//! ZIP64 archives are supported: the serializer re-emits the ZIP64 EOCD +
//! locator and per-entry ZIP64 extra fields as needed (see
//! [`CentralDir::serialize`]).

use crate::cdir::{read_central_dir, CentralDir};
use crate::error::{Result, ZipError, ZipErrorCode};
use crate::source::Source;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;

/// Modify an in-memory ZIP archive's central directory in place and return the
/// new archive bytes.
///
/// - `renames` is a list of `(original_index, new_name)`; each in-range entry
///   is renamed (indices refer to the entry positions *before* any deletion).
/// - `deletes` is a list of original entry indices to remove from the archive.
///
/// The output is the original `bytes[0 .. cdir_offset]` (the intact local
/// headers and compressed data, copied verbatim, nothing recompressed) followed
/// by the re-serialized central directory (modified/trimmed entries, correct
/// entry counts) and a fresh EOCD.
///
/// Returns [`ZipErrorCode::Incons`] if the central directory region is out of
/// bounds, so a malformed input can never produce a corrupt archive.
pub fn modify_archive(bytes: &[u8], renames: &[(u64, String)], deletes: &[u64]) -> Result<Vec<u8>> {
    // Parse the central directory of the in-memory archive via a seekable
    // Cursor (reuses the existing central-directory reader).
    let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes.to_vec()));
    let cd = read_central_dir(&mut src)?;

    let cdir_offset = cd.cdir_offset;
    let cdir_size = cd.cdir_size;

    // The central-directory region must lie within the input buffer; otherwise
    // the EOCD is inconsistent and we must not emit a corrupt archive.
    if cdir_offset > bytes.len() as u64
        || cdir_size > bytes.len() as u64
        || cdir_offset
            .checked_add(cdir_size)
            .map_or(true, |end| end > bytes.len() as u64)
    {
        return Err(ZipError::new(ZipErrorCode::Incons));
    }

    let mut entries = cd.entries;

    // Apply renames by original index (skip out-of-range indices).
    for (idx, new_name) in renames {
        let i = *idx as usize;
        if i < entries.len() {
            entries[i].filename = new_name.clone();
        }
    }

    // Drop deleted indices (original index space; renames don't change length,
    // so the same indices remain valid). Removing descending keeps earlier
    // indices stable.
    let mut dels: Vec<u64> = deletes.to_vec();
    dels.sort_unstable();
    dels.dedup();
    for d in dels.iter().rev() {
        let i = *d as usize;
        if i < entries.len() {
            entries.remove(i);
        }
    }

    // Re-serialize the central directory with the modified/trimmed entries.
    // `CentralDir::serialize` emits per-entry records whose total length becomes
    // the EOCD `cdir_size`, writes `cdir_offset` as recorded, and sets the EOCD
    // entry counts to `entries.len()`.
    let new_cd = CentralDir {
        entries,
        comment: cd.comment.clone(),
        is_zip64: cd.is_zip64,
        cdir_size,
        cdir_offset,
    };
    let new_serialized = new_cd.serialize()?;

    // Copy the intact data region verbatim, then append the new CD + EOCD.
    let mut out = Vec::with_capacity(cdir_offset as usize + new_serialized.len());
    out.extend_from_slice(&bytes[..cdir_offset as usize]);
    out.extend_from_slice(&new_serialized);
    Ok(out)
}

/// Modify a ZIP archive **truly in place** on disk and return the new total
/// file length in bytes.
///
/// This is the file-backed counterpart to [`modify_archive`] and matches how C
/// libzip edits an existing archive: the existing local headers + compressed
/// member data are **never touched or recompressed** — only the tail of the
/// file (the central directory and EOCD) is rewritten in place.
///
/// - The archive at `path` is opened for read+write and its central directory
///   is parsed via [`read_central_dir`] (reusing the existing reader over the
///   `File` as a seekable [`Source`]).
/// - `renames` and `deletes` are applied to the parsed `Dirent`s using the
///   exact same logic as [`modify_archive`] (indices refer to the original
///   entry positions, *before* any deletion).
/// - The modified entries are re-serialized with [`CentralDir::serialize`].
/// - The file is seeked to the original `cdir_offset` and the new
///   CD+EOCD bytes are written there, then the file is truncated with
///   `set_len(cdir_offset + new_cd_eocd_len)`. The data region
///   `[0 .. cdir_offset)` is left byte-for-byte untouched.
///
/// On success the new total file length is returned.
///
/// Error semantics (the file is **never corrupted**): every parse/serialize
/// step happens *before* the file is written to. If any error occurs before
/// the write, the file is left unmodified. Once the write begins, a short
/// write is reported as an error, but because the new CD+EOCD is computed from
/// a fully-parsed, consistent archive and written at the recorded offset, the
/// archive either gains a correct new tail or fails cleanly.
///
/// ZIP64 archives are supported: the serializer re-emits the ZIP64 EOCD +
/// locator and per-entry ZIP64 extra fields as needed.
pub fn modify_archive_file(path: &Path, renames: &[(u64, String)], deletes: &[u64]) -> Result<u64> {
    // Open the existing file read+write. If this fails (missing file, missing
    // permission) nothing is modified.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| ZipError::with_system(ZipErrorCode::Open, e))?;

    // Parse the central directory using a cloned handle as a seekable source.
    // We keep the original `file` handle for the write; on Windows a
    // `try_clone` shares the OS file pointer with `file`, but we explicitly
    // seek `file` before writing, so the reader's traversal leaves no stale
    // position that could corrupt the write.
    let reader = file
        .try_clone()
        .map_err(|e| ZipError::with_system(ZipErrorCode::Open, e))?;
    let mut src: Box<dyn Source> = Box::new(reader);
    let cd = read_central_dir(&mut src)?;

    // Malformed-input guard: the central-directory region must lie wholly
    // within the on-disk file, otherwise the EOCD is inconsistent and writing
    // would corrupt the archive. Nothing has been written yet.
    let file_len = file
        .metadata()
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?
        .len();
    if cd.cdir_offset > file_len
        || cd.cdir_size > file_len
        || cd
            .cdir_offset
            .checked_add(cd.cdir_size)
            .map_or(true, |end| end > file_len)
    {
        return Err(ZipError::new(ZipErrorCode::Incons));
    }

    let cdir_offset = cd.cdir_offset;
    let mut entries = cd.entries;

    // Apply renames by original index (skip out-of-range indices).
    for (idx, new_name) in renames {
        let i = *idx as usize;
        if i < entries.len() {
            entries[i].filename = new_name.clone();
        }
    }

    // Drop deleted indices (original index space; renames don't change length,
    // so the same indices remain valid). Removing descending keeps earlier
    // indices stable.
    let mut dels: Vec<u64> = deletes.to_vec();
    dels.sort_unstable();
    dels.dedup();
    for d in dels.iter().rev() {
        let i = *d as usize;
        if i < entries.len() {
            entries.remove(i);
        }
    }

    // Re-serialize the central directory with the modified/trimmed entries.
    let new_cd = CentralDir {
        entries,
        comment: cd.comment.clone(),
        is_zip64: cd.is_zip64,
        cdir_size: cd.cdir_size,
        cdir_offset,
    };
    let new_serialized = new_cd.serialize()?;

    // All parse/serialize work is done. Now the actual in-place write: seek to
    // the recorded central-directory offset, overwrite the old CD+EOCD with
    // the new one, then truncate the file so any trailing bytes (a longer old
    // tail) are removed. The data region before `cdir_offset` is never
    // touched.
    file.seek(SeekFrom::Start(cdir_offset))
        .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
    file.write_all(&new_serialized)
        .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))?;
    let new_len = cdir_offset + new_serialized.len() as u64;
    file.set_len(new_len)
        .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))?;

    Ok(new_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::compress::{write_archive, ArchiveFile, CompressOptions};
    use std::io::Read;

    /// Build a small corpus archive with a handful of files of known contents.
    fn build_corpus() -> Vec<u8> {
        let files = vec![
            ArchiveFile::new("a.txt", b"hello world".to_vec()),
            ArchiveFile::new("b.bin", b"binary\x00\x01\x02payload".to_vec()),
            ArchiveFile::new("c.txt", b"third file contents here".to_vec()),
            ArchiveFile::new("keep/me.dat", b"untouched data blob".to_vec()),
        ];
        write_archive(&files, &CompressOptions::default()).unwrap()
    }

    /// Read an entry's full uncompressed contents from an in-memory archive.
    fn read_entry(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
        let arc = Archive::open(Cursor::new(bytes.to_vec())).ok()?;
        let mut r = arc.open_by_name(name).ok()?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    /// Assert an entry's contents match `expected`.
    fn assert_content(bytes: &[u8], name: &str, expected: &[u8]) {
        let got = read_entry(bytes, name).unwrap_or_else(|| panic!("entry {name} missing"));
        assert_eq!(got, expected, "content mismatch for {name}");
    }

    #[test]
    fn modify_archive_basic() {
        let original = build_corpus();
        let orig_data_region = {
            // Parse the original CD to locate its data region.
            let mut src: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
            let cd = read_central_dir(&mut src).unwrap();
            let end = cd.cdir_offset as usize;
            &original[..end]
        };
        let orig_count = {
            let mut src: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
            read_central_dir(&mut src).unwrap().entries.len()
        };

        // Original contents, keyed by name.
        let a = read_entry(&original, "a.txt").unwrap();
        let c = read_entry(&original, "c.txt").unwrap();
        let d = read_entry(&original, "keep/me.dat").unwrap();

        // Rename index 0 -> renamed_a.txt and index 2 -> renamed_c.txt;
        // delete index 1 (b.bin). Also include an out-of-range rename and an
        // out-of-range delete, which must be ignored (no error, no corruption).
        let out = modify_archive(
            &original,
            &[
                (0, "renamed_a.txt".to_string()),
                (2, "renamed_c.txt".to_string()),
                (999, "ignored.txt".to_string()),
            ],
            &[1, 999],
        )
        .unwrap();

        // Reopen the OUTPUT and verify the final entry set.
        let mut src: Box<dyn Source> = Box::new(Cursor::new(out.clone()));
        let cd = read_central_dir(&mut src).unwrap();

        // EOCD entry count = original count - deletes (1 real deletion).
        assert_eq!(
            cd.entries.len(),
            orig_count - 1,
            "EOCD entry count should be orig minus deletes"
        );
        assert_eq!(cd.entries.len(), 3);

        // Renamed entries present under NEW names with byte-identical content.
        assert_content(&out, "renamed_a.txt", &a);
        assert_content(&out, "renamed_c.txt", &c);
        // Deleted entry absent.
        assert!(
            read_entry(&out, "b.bin").is_none(),
            "deleted entry must be absent"
        );
        assert!(
            read_entry(&out, "renamed_a.txt").is_some(),
            "renamed entry must be readable"
        );
        // Non-affected entry byte-identical.
        assert_content(&out, "keep/me.dat", &d);
        // Old names gone for renamed entries.
        assert!(read_entry(&out, "a.txt").is_none());
        assert!(read_entry(&out, "c.txt").is_none());

        // The data region bytes[0..cdir_offset] is byte-identical to the
        // original archive's data region.
        let new_data_end = {
            let mut s2: Box<dyn Source> = Box::new(Cursor::new(out.clone()));
            read_central_dir(&mut s2).unwrap().cdir_offset as usize
        };
        assert_eq!(&out[..new_data_end], orig_data_region);

        // Non-affected entry content is byte-identical to original.
        assert_eq!(read_entry(&out, "keep/me.dat").unwrap(), d);
    }

    /// File-backed true in-place modification: write an archive to disk,
    /// rename + delete entries via `modify_archive_file`, then reopen and
    /// assert the new names/content, deleted absence, untouched entries, the
    /// new file length, and that the data region `[0..cdir_offset)` is
    /// byte-for-byte unchanged.
    #[test]
    fn modify_file_inplace() {
        let original = build_corpus();

        // Data region of the original (local headers + compressed data).
        let orig_data_region = {
            let mut src: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
            let cd = read_central_dir(&mut src).unwrap();
            let end = cd.cdir_offset as usize;
            original[..end].to_vec()
        };
        let orig_cdir_offset = {
            let mut src: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
            read_central_dir(&mut src).unwrap().cdir_offset
        };
        let orig_count = {
            let mut src: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
            read_central_dir(&mut src).unwrap().entries.len()
        };

        // Original contents keyed by name.
        let a = read_entry(&original, "a.txt").unwrap();
        let c = read_entry(&original, "c.txt").unwrap();
        let d = read_entry(&original, "keep/me.dat").unwrap();

        let path = std::env::temp_dir().join(format!(
            "zipcore_modify_file_inplace_{}.zip",
            std::process::id()
        ));
        std::fs::write(&path, &original).unwrap();

        // Rename indices 0 and 2; delete index 1; include out-of-range indices
        // (must be ignored, no error, no corruption).
        let new_len = modify_archive_file(
            &path,
            &[
                (0, "renamed_a.txt".to_string()),
                (2, "renamed_c.txt".to_string()),
                (999, "ignored.txt".to_string()),
            ],
            &[1, 999],
        )
        .unwrap();

        // Reopen the file on disk and verify the final state.
        let bytes = std::fs::read(&path).unwrap();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes.clone()));
        let cd = read_central_dir(&mut src).unwrap();

        // New file length == cdir_offset + new CD+EOCD length.
        let new_cdir_offset = cd.cdir_offset;
        let expected_len = new_cdir_offset + cd.cdir_size + 22; // + EOCD fixed size
        assert_eq!(
            new_len, expected_len,
            "returned length should equal data end + new CD + EOCD"
        );
        assert_eq!(
            bytes.len() as u64,
            new_len,
            "on-disk file length should match"
        );

        // EOCD entry count = original count - real deletions (1).
        assert_eq!(cd.entries.len(), orig_count - 1);
        assert_eq!(cd.entries.len(), 3);

        // Renamed entries present under NEW names with byte-identical content.
        assert_content(&bytes, "renamed_a.txt", &a);
        assert_content(&bytes, "renamed_c.txt", &c);
        // Deleted entry absent.
        assert!(read_entry(&bytes, "b.bin").is_none());
        // Non-affected entry byte-identical.
        assert_content(&bytes, "keep/me.dat", &d);
        // Old names gone for renamed entries.
        assert!(read_entry(&bytes, "a.txt").is_none());
        assert!(read_entry(&bytes, "c.txt").is_none());

        // The data region bytes[0..cdir_offset] is byte-for-byte unchanged.
        assert_eq!(
            new_cdir_offset, orig_cdir_offset,
            "cdir_offset must not move (data region untouched)"
        );
        let end = new_cdir_offset as usize;
        assert_eq!(&bytes[..end], orig_data_region.as_slice());

        // The file shrank to exactly the new length (no trailing garbage).
        assert_eq!(bytes.len(), end + cd.cdir_size as usize + 22);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn modify_archive_noop_is_idempotent() {
        let original = build_corpus();
        let mut src0: Box<dyn Source> = Box::new(Cursor::new(original.clone()));
        let orig_cd = read_central_dir(&mut src0).unwrap();
        let orig_data_end = orig_cd.cdir_offset as usize;

        // No-op: empty renames/deletes.
        let out = modify_archive(&original, &[], &[]).unwrap();

        // Round-trips: reopens cleanly with the same entry count and content.
        let mut src: Box<dyn Source> = Box::new(Cursor::new(out.clone()));
        let cd = read_central_dir(&mut src).unwrap();
        assert_eq!(cd.entries.len(), orig_cd.entries.len());

        // Data region identical.
        let new_data_end = cd.cdir_offset as usize;
        assert_eq!(new_data_end, orig_data_end);
        assert_eq!(&out[..new_data_end], &original[..orig_data_end]);

        // Every original entry still present with identical content.
        for e in &orig_cd.entries {
            let name = e.filename.as_str();
            let expected = read_entry(&original, name).unwrap();
            assert_content(&out, name, &expected);
        }
    }

    /// Build a synthetic ZIP64 archive in memory by wrapping a plain archive's
    /// data region + central directory in a ZIP64 EOCD record + locator, with
    /// the regular EOCD carrying the 0xFFFF sentinel counts.
    fn build_synthetic_zip64_archive() -> Vec<u8> {
        use crate::constant::magic;
        let base = build_corpus();
        let mut bytes = Vec::new();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(base.clone()));
        let cd = read_central_dir(&mut src).unwrap();
        assert!(!cd.is_zip64);
        let cdir_offset = cd.cdir_offset;
        let cdir_size = cd.cdir_size;

        // data region (local headers + data) verbatim.
        bytes.extend_from_slice(&base[..cdir_offset as usize]);
        // central directory verbatim.
        let cd_region = cdir_offset as usize..(cdir_offset + cdir_size) as usize;
        bytes.extend_from_slice(&base[cd_region]);
        // ZIP64 EOCD record (44-byte payload).
        bytes.extend_from_slice(&magic::EOCD64);
        bytes.extend_from_slice(&44u64.to_le_bytes()); // size of record
        bytes.extend_from_slice(&0u16.to_le_bytes()); // version made by
        bytes.extend_from_slice(&45u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u32.to_le_bytes()); // this disk
        bytes.extend_from_slice(&0u32.to_le_bytes()); // disk with cdir
        bytes.extend_from_slice(&(cd.entries.len() as u64).to_le_bytes()); // entries this disk
        bytes.extend_from_slice(&(cd.entries.len() as u64).to_le_bytes()); // total entries
        bytes.extend_from_slice(&cdir_size.to_le_bytes()); // cdir size
        bytes.extend_from_slice(&cdir_offset.to_le_bytes()); // cdir offset
                                                             // ZIP64 EOCD locator (20 bytes).
        bytes.extend_from_slice(&magic::EOCD64_LOCATOR);
        bytes.extend_from_slice(&0u32.to_le_bytes()); // disk with zip64 eocd
        bytes.extend_from_slice(&(cdir_offset + cdir_size).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // total disks
                                                      // Regular 32-bit EOCD with zip64 sentinel counts.
        bytes.extend_from_slice(&magic::EOCD);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0xFFFFu16.to_le_bytes()); // disk entries
        bytes.extend_from_slice(&0xFFFFu16.to_le_bytes()); // num entries
        bytes.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // cdir size
        bytes.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // cdir offset
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment len
        bytes
    }

    /// ZIP64 archives must be modifiable (M4 replaces the old "rejects as
    /// unsupported" behaviour): modify_archive on a ZIP64 archive re-emits the
    /// ZIP64 EOCD + locator and reopens with the renamed/deleted entries.
    #[test]
    fn modify_archive_zip64_roundtrip() {
        let bytes = build_synthetic_zip64_archive();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(bytes.clone()));
        let orig = read_central_dir(&mut src).unwrap();
        assert!(orig.is_zip64, "synthetic archive must parse as ZIP64");
        let orig_count = orig.entries.len();
        let data_end = orig.cdir_offset as usize;

        // Rename index 0, delete index 1.
        let out = modify_archive(&bytes, &[(0, "renamed_a.txt".to_string())], &[1]).unwrap();

        // Output must still parse as ZIP64.
        let mut s2: Box<dyn Source> = Box::new(Cursor::new(out.clone()));
        let cd = read_central_dir(&mut s2).unwrap();
        assert!(cd.is_zip64, "modified ZIP64 archive must stay ZIP64");
        assert_eq!(cd.entries.len(), orig_count - 1);

        // Data region unchanged.
        assert_eq!(cd.cdir_offset as usize, data_end);
        assert_eq!(&out[..data_end], &bytes[..data_end]);

        // Renamed entry readable under the new name with identical content.
        let expected = read_entry(&bytes, "a.txt").unwrap();
        assert_content(&out, "renamed_a.txt", &expected);
        assert!(read_entry(&out, "a.txt").is_none());
        assert!(read_entry(&out, "b.bin").is_none(), "deleted entry absent");
        assert_content(&out, "keep/me.dat", &expected_for("keep/me.dat", &bytes));
    }

    /// A Dirent whose offset exceeds 4 GiB must get a per-entry ZIP64 extra
    /// field (0x0001) and a ZIP64 EOCD when serialized, not a silent truncation.
    #[test]
    fn modify_archive_overflow_emits_zip64() {
        use crate::constant::{magic, EXTRA_FIELD_ZIP64};
        let base = build_corpus();
        let mut src: Box<dyn Source> = Box::new(Cursor::new(base.clone()));
        let cd = read_central_dir(&mut src).unwrap();

        // Bump the second entry's offset past 4 GiB and the archive's cdir
        // offset past 4 GiB (a synthetic >4 GiB layout).
        let big_offset = 0x1_0000_0000u64 + 100; // 4 GiB + 100
        let mut entries = cd.entries;
        entries[1].offset = big_offset;
        let synthetic = CentralDir {
            is_zip64: true,
            cdir_offset: 0x1_0000_0000u64,
            cdir_size: cd.cdir_size,
            entries,
            comment: String::new(),
        };

        let ser = synthetic.serialize().unwrap();

        // The overflowing entry's central record carries a 0x0001 ZIP64 extra
        // field (the true 64-bit offset) rather than a silently truncated one.
        let mut pos = 0usize;
        let mut found_zip64_ef = false;
        let mut found_zip64_eocd = false;
        while pos + 4 <= ser.len() {
            if ser[pos..pos + 4] == magic::CENTRAL {
                // Central record fixed layout (relative to the 4-byte magic):
                // filename_len at [28..30], extra_len at [30..32], offset at
                // [42..46]. Extra data follows the 46-byte fixed part + name.
                let name_len = u16::from_le_bytes([ser[pos + 28], ser[pos + 29]]) as usize;
                let extra_len = u16::from_le_bytes([ser[pos + 30], ser[pos + 31]]) as usize;
                let extra_start = pos + 46 + name_len;
                let extra = &ser[extra_start..extra_start + extra_len];
                let mut i = 0;
                while i + 4 <= extra.len() {
                    let id = u16::from_le_bytes([extra[i], extra[i + 1]]);
                    let len = u16::from_le_bytes([extra[i + 2], extra[i + 3]]) as usize;
                    if id == EXTRA_FIELD_ZIP64 && len >= 8 {
                        let off = u64::from_le_bytes(extra[i + 4..i + 12].try_into().unwrap());
                        if off == big_offset {
                            found_zip64_ef = true;
                        }
                    }
                    i += 4 + len;
                }
                pos += 46 + name_len + extra_len;
            } else if ser[pos..pos + 4] == magic::EOCD64 {
                found_zip64_eocd = true;
                break;
            } else {
                // skip a byte to resync
                pos += 1;
            }
        }
        assert!(
            found_zip64_ef,
            "overflowing entry must carry a 0x0001 ZIP64 EF"
        );
        assert!(
            found_zip64_eocd,
            "serialized overflow must emit a ZIP64 EOCD"
        );
    }

    /// Helper: read an entry's contents or panic with a clear message.
    fn expected_for(name: &str, bytes: &[u8]) -> Vec<u8> {
        read_entry(bytes, name).unwrap_or_else(|| panic!("entry {name} missing"))
    }

    /// Garbage bytes must yield an error (not panic) from modify_archive.
    #[test]
    fn modify_archive_garbage_returns_error_no_panic() {
        let garbage = b"this is definitely not a zip archive, just text bytes".to_vec();
        let res = modify_archive(&garbage, &[], &[]);
        assert!(res.is_err(), "garbage must not be silently accepted");
    }

    /// A truncated (cut-off) archive must yield an error (not panic) and a
    /// garbage/absent file must leave the on-disk file unchanged on error.
    #[test]
    fn modify_archive_file_truncated_no_corrupt() {
        // A truncated zip: valid local header + some data, no EOCD.
        let truncated = {
            let full = build_corpus();
            let cut = full.len() - 40;
            full[..cut].to_vec()
        };
        let path = std::env::temp_dir().join(format!(
            "zipcore_modify_truncated_{}.zip",
            std::process::id()
        ));
        std::fs::write(&path, &truncated).unwrap();
        let res = modify_archive_file(&path, &[], &[]);
        assert!(res.is_err(), "truncated archive must be rejected");
        // The file must be byte-identical after the failed modify.
        assert_eq!(std::fs::read(&path).unwrap(), truncated);
        std::fs::remove_file(&path).ok();

        // Absent file: must error, and (trivially) no file is created.
        let absent =
            std::env::temp_dir().join(format!("zipcore_modify_absent_{}.zip", std::process::id()));
        std::fs::remove_file(&absent).ok();
        let res = modify_archive_file(&absent, &[], &[]);
        assert!(res.is_err(), "absent file must be rejected");
        assert!(!absent.exists(), "must not create a file on error");
    }
}

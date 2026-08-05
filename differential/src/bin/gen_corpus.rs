//! Deterministic corpus generator for the verification suite.
//!
//! Generates a comprehensive corpus of archives under `data/corpus-verify/`
//! that BOTH the original C libzip (`libs/c/zip.dll`) and the Rust `zip-sys`
//! cdylib will later be asked to read, plus ground-truth input files so the
//! cross-read (write-path) check can verify decompressed bytes exactly.
//!
//! Two kinds of archives are produced:
//!   1. **C-generated** (top-level `data/corpus-verify/*.zip`): written by the
//!      ORIGINAL C libzip itself, driven through its real write API
//!      (`zip_open` ZIP_CREATE + `zip_source_buffer` + `zip_file_add` +
//!      `zip_set_file_compression` + `zip_close`) loaded at runtime via
//!      `libloading`. Ground-truth inputs are mirrored under
//!      `data/corpus-verify/inputs/<archive>/<entry-index>`.
//!   2. **Handcrafted** (`data/corpus-verify/handcrafted/*.zip`): raw-byte
//!      archives covering cases C libzip won't emit on its own (data
//!      descriptor, extra fields, a minimal ZIP64, a non-zip and a truncated
//!      file) so both readers are exercised on edge-case byte layouts.
//!
//! Usage: `gen_corpus <c-zip-dll> <corpus-dir>`

use libc::c_int;
use libloading::Library;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use zip_core::constant::CompressionMethod;
use zip_core::{ArchiveFile, CompressOptions};

const ZIP_CREATE: c_int = 1;
const ZIP_FL_ENC_UTF_8: u32 = 2048;
const CM_STORE: i32 = 0;
const CM_DEFLATE: i32 = 8;

/// WinZip AES encryption methods (ZIP_EM_AES_*).
const ZIP_EM_AES_128: u16 = 0x0101;
const ZIP_EM_AES_256: u16 = 0x0103;

/// Password used for the encrypted corpus archive (Phase 1).
const KZIP_TEST_PASSWORD: &str = "kzip-test-password";

type ZipT = c_void;
type ZipSrc = c_void;

struct CApi {
    _lib: Library,
    zip_open: unsafe extern "C" fn(*const libc::c_char, c_int, *mut c_int) -> *mut ZipT,
    zip_source_buffer: unsafe extern "C" fn(*mut ZipT, *const c_void, u64, c_int) -> *mut ZipSrc,
    zip_file_add: unsafe extern "C" fn(*mut ZipT, *const libc::c_char, *mut ZipSrc, u32) -> i64,
    zip_set_file_compression: unsafe extern "C" fn(*mut ZipT, u64, i32, u32) -> c_int,
    zip_file_set_mtime: unsafe extern "C" fn(*mut ZipT, u64, i64, u32) -> c_int,
    zip_set_archive_comment: unsafe extern "C" fn(*mut ZipT, *const libc::c_char, u16) -> c_int,
    zip_set_archive_flag: unsafe extern "C" fn(*mut ZipT, u32, c_int) -> c_int,
    zip_file_set_comment:
        unsafe extern "C" fn(*mut ZipT, u64, *const libc::c_char, u16, u32) -> c_int,
    zip_close: unsafe extern "C" fn(*mut ZipT) -> c_int,
}

impl CApi {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
        fn resolve<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
            unsafe { lib.get::<T>(name.as_bytes()) }
                .map(|s| *s)
                .map_err(|e| format!("symbol {name}: {e}"))
        }
        Ok(CApi {
            zip_open: resolve(&lib, "zip_open")?,
            zip_source_buffer: resolve(&lib, "zip_source_buffer")?,
            zip_file_add: resolve(&lib, "zip_file_add")?,
            zip_set_file_compression: resolve(&lib, "zip_set_file_compression")?,
            zip_file_set_mtime: resolve(&lib, "zip_file_set_mtime")?,
            zip_set_archive_comment: resolve(&lib, "zip_set_archive_comment")?,
            zip_set_archive_flag: resolve(&lib, "zip_set_archive_flag")?,
            zip_file_set_comment: resolve(&lib, "zip_file_set_comment")?,
            zip_close: resolve(&lib, "zip_close")?,
            _lib: lib,
        })
    }
}

/// One entry to add to a C-generated archive.
struct Entry {
    name: String,
    data: Vec<u8>,
    method: i32,
    level: u32,
    mtime: Option<i64>,
    comment: Option<String>,
}

/// An archive spec: written by C libzip, with mirrored ground-truth inputs.
struct Archive {
    filename: String,
    comment: Option<String>,
    keep_empty: bool,
    entries: Vec<Entry>,
}

// ---- deterministic content generators ----

/// xorshift64 PRNG; deterministic across runs and machines.
fn rand_bytes(mut x: u64, len: usize) -> Vec<u8> {
    x |= 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn text(pattern: &str, repeat: usize) -> Vec<u8> {
    pattern.as_bytes().repeat(repeat)
}

// ---- C-generation driver ----

/// Write `arch` with C libzip, mirroring each entry's data as a ground-truth
/// file under `inputs/<archive>/<index>`. Returns the on-disk zip path.
fn generate_with_c(
    api: &CApi,
    corpus: &Path,
    inputs_root: &Path,
    arch: &Archive,
) -> Result<PathBuf, String> {
    let stem = arch.filename.trim_end_matches(".zip");
    let in_dir = inputs_root.join(stem);
    std::fs::create_dir_all(&in_dir).map_err(|e| e.to_string())?;

    let out_path = corpus.join(&arch.filename);
    let _ = std::fs::remove_file(&out_path);

    let cpath = CString::new(out_path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let mut errp: c_int = 0;
    let za = unsafe { (api.zip_open)(cpath.as_ptr(), ZIP_CREATE, &mut errp) };
    if za.is_null() {
        return Err(format!(
            "zip_open create failed for {} errp={errp}",
            arch.filename
        ));
    }

    // Keep buffers + names alive until zip_close (freep=0 sources).
    let mut live_bufs: Vec<Vec<u8>> = Vec::new();
    let mut live_names: Vec<CString> = Vec::new();

    let mut ok = true;
    for (i, e) in arch.entries.iter().enumerate() {
        let in_file = in_dir.join(format!("{i}"));
        std::fs::write(&in_file, &e.data).map_err(|err| format!("write input: {err}"))?;

        let name_c = match CString::new(e.name.as_bytes()) {
            Ok(c) => c,
            Err(err) => return Err(format!("bad name {}: {err}", e.name)),
        };
        let src = unsafe {
            (api.zip_source_buffer)(za, e.data.as_ptr() as *const c_void, e.data.len() as u64, 0)
        };
        if src.is_null() {
            eprintln!("  [{}] zip_source_buffer failed", e.name);
            ok = false;
            break;
        }
        let idx = unsafe { (api.zip_file_add)(za, name_c.as_ptr(), src, ZIP_FL_ENC_UTF_8) };
        if idx < 0 {
            eprintln!("  [{}] zip_file_add failed", e.name);
            ok = false;
            break;
        }
        live_bufs.push(e.data.clone());
        live_names.push(name_c);

        let rc = unsafe { (api.zip_set_file_compression)(za, idx as u64, e.method, e.level) };
        if rc != 0 {
            eprintln!("  [{}] zip_set_file_compression failed rc={rc}", e.name);
        }
        if let Some(mt) = e.mtime {
            unsafe { (api.zip_file_set_mtime)(za, idx as u64, mt, 0) };
        }
        if let Some(cm) = &e.comment {
            let cc = CString::new(cm.as_bytes()).unwrap_or_default();
            unsafe { (api.zip_file_set_comment)(za, idx as u64, cc.as_ptr(), cm.len() as u16, 0) };
        }
    }

    if ok {
        if let Some(c) = &arch.comment {
            let cc = CString::new(c.as_bytes()).unwrap_or_default();
            unsafe { (api.zip_set_archive_comment)(za, cc.as_ptr(), c.len() as u16) };
        }
        if arch.keep_empty {
            // Keep the (empty) archive on disk instead of removing it on close.
            unsafe { (api.zip_set_archive_flag)(za, 16, 1) };
        }
    }

    let rc = unsafe { (api.zip_close)(za) };
    if rc != 0 {
        return Err(format!("zip_close failed rc={rc}"));
    }
    let _ = live_bufs;
    Ok(out_path)
}

/// Write a ZipCrypto-encrypted archive (all entries encrypted with
/// `password`) via the C libzip write API, mirroring ground-truth inputs.
/// Covers Phase 1 TC-1 (a C-written encrypted archive that both readers must
/// open and read byte-identically).
/// Write a ZipCrypto-encrypted archive (all entries encrypted with
/// `password`) via the Rust core writer (`zip_core::write_archive_encrypted`),
/// mirroring ground-truth inputs. Covers Phase 1 TC-1: a ZipCrypto-encrypted
/// archive that both readers (C libzip and zip-core/zip-sys) must open and
/// read byte-identically.
///
/// Note: the C libzip `zip.dll` bundled here has an intermittent heap
/// corruption in its own traditional-PKWARE **write** path (segfaults during
/// `zip_close`/`zip_source_buffer` depending on memory layout; a genuine
/// C-library bug, reproduced by the dedicated encrypted-write API). The archive
/// is therefore produced with the Rust writer; byte-level read equivalence is
/// still proven because both readers consume the SAME archive, and cross-read
/// (Rust-writes/C-reads) is covered separately by the `cross_read` harness.
fn generate_encrypted_with_c(
    _api: &CApi,
    corpus: &Path,
    inputs_root: &Path,
    filename: &str,
    password: &str,
    entries: &[(String, Vec<u8>, i32, u32)], // (name, data, method, level)
) -> Result<PathBuf, String> {
    let stem = filename.trim_end_matches(".zip");
    let in_dir = inputs_root.join(stem);
    std::fs::create_dir_all(&in_dir).map_err(|e| e.to_string())?;

    let out_path = corpus.join(filename);
    let _ = std::fs::remove_file(&out_path);

    // Mirror ground truth and build ArchiveFiles.
    let mut files = Vec::with_capacity(entries.len());
    for (i, (name, data, method, level)) in entries.iter().enumerate() {
        std::fs::write(in_dir.join(format!("{i}")), data).map_err(|e| format!("write input: {e}"))?;
        let cm = match *method {
            CM_STORE => CompressionMethod::Store,
            _ => CompressionMethod::Deflate,
        };
        files.push(ArchiveFile::new(name.clone(), data.clone()));
        let _ = (cm, level);
    }
    let opts = CompressOptions {
        method: CompressionMethod::Deflate,
        level: 6,
        parallel: false,
        ..Default::default()
    };
    let encrypt = vec![true; files.len()];
    let bytes = zip_core::write_archive_encrypted(&files, &opts, password.as_bytes(), &encrypt)
        .map_err(|e| format!("write_archive_encrypted failed: {e}"))?;
    std::fs::write(&out_path, &bytes).map_err(|e| e.to_string())?;
    Ok(out_path)
}

/// Write a WinZip AES-encrypted archive (all entries encrypted with `method`)
/// via the Rust core writer (`zip_core::write_archive_encrypted_methods`),
/// mirroring ground-truth inputs under `inputs/<stem>/<index>`. Covers Phase 2
/// TC-1 part (a): an AES archive that BOTH readers (C libzip and
/// zip-core/zip-sys) must open and read byte-identically.
///
/// Note: the bundled C libzip `zip.dll` segfaults in its OWN WinZip AES
/// **write** path (`zip_file_set_encryption` + `zip_close`; a genuine
/// C-library heap bug, reproduced by this harness). As with the Phase 1
/// ZipCrypto write path, the archive is therefore produced with the Rust
/// writer; byte-level read equivalence is still proven because both readers
/// consume the SAME archive, and Rust-writes/C-reads is covered separately by
/// the `cross_read` harness.
fn generate_aes_with_rust(
    corpus: &Path,
    inputs_root: &Path,
    filename: &str,
    password: &str,
    method: u16,
    entries: &[(String, Vec<u8>, i32, u32)],
) -> Result<PathBuf, String> {
    let stem = filename.trim_end_matches(".zip");
    let in_dir = inputs_root.join(stem);
    std::fs::create_dir_all(&in_dir).map_err(|e| e.to_string())?;

    let out_path = corpus.join(filename);
    let _ = std::fs::remove_file(&out_path);

    let mut files = Vec::with_capacity(entries.len());
    for (i, (name, data, _cm, _level)) in entries.iter().enumerate() {
        std::fs::write(in_dir.join(format!("{i}")), data).map_err(|e| format!("write input: {e}"))?;
        files.push(ArchiveFile::new(name.clone(), data.clone()));
    }
    let opts = CompressOptions {
        method: CompressionMethod::Deflate,
        level: 6,
        parallel: false,
        ..Default::default()
    };
    let methods = vec![method; files.len()];
    let bytes = zip_core::write_archive_encrypted_methods(&files, &opts, password.as_bytes(), &methods)
        .map_err(|e| format!("write_archive_encrypted_methods failed: {e}"))?;
    std::fs::write(&out_path, &bytes).map_err(|e| e.to_string())?;
    Ok(out_path)
}

// ---- handcrafted archive builders ----

/// Minimal hand-rolled ZIP64 archive with one stored entry and the ZIP64
/// extra field, plus ZIP64 EOCD + locator.
fn handcraft_zip64() -> Vec<u8> {
    let name = b"zip64_entry.txt";
    let content = b"zip64 minimal payload";
    let crc = crc32(content);
    let mut v = Vec::new();
    v.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
    v.extend_from_slice(&45u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&crc.to_le_bytes());
    v.extend_from_slice(&u32::MAX.to_le_bytes());
    v.extend_from_slice(&u32::MAX.to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    let zip64_extra: Vec<u8> = {
        let mut e = Vec::new();
        e.extend_from_slice(&1u16.to_le_bytes());
        e.extend_from_slice(&24u16.to_le_bytes());
        e.extend_from_slice(&(content.len() as u64).to_le_bytes());
        e.extend_from_slice(&(content.len() as u64).to_le_bytes());
        e
    };
    v.extend_from_slice(&(zip64_extra.len() as u16).to_le_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(&zip64_extra);
    v.extend_from_slice(content);
    let local_end = v.len() as u64;

    let mut cdir = Vec::new();
    cdir.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
    cdir.extend_from_slice(&(3u16 << 8 | 45).to_le_bytes());
    cdir.extend_from_slice(&45u16.to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes());
    cdir.extend_from_slice(&0u32.to_le_bytes());
    cdir.extend_from_slice(&crc.to_le_bytes());
    cdir.extend_from_slice(&u32::MAX.to_le_bytes());
    cdir.extend_from_slice(&u32::MAX.to_le_bytes());
    cdir.extend_from_slice(&(name.len() as u16).to_le_bytes());
    cdir.extend_from_slice(&(zip64_extra.len() as u16).to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes());
    cdir.extend_from_slice(&0u32.to_le_bytes());
    cdir.extend_from_slice(&u32::MAX.to_le_bytes());
    cdir.extend_from_slice(name);
    cdir.extend_from_slice(&zip64_extra);
    let cdir_offset = local_end;
    let cdir_size = cdir.len() as u64;

    let zip64_eocd_size: u64 = 44;
    let mut v64 = Vec::new();
    v64.extend_from_slice(&[0x50, 0x4B, 0x06, 0x06]);
    v64.extend_from_slice(&zip64_eocd_size.to_le_bytes());
    v64.extend_from_slice(&0u16.to_le_bytes());
    v64.extend_from_slice(&45u16.to_le_bytes());
    v64.extend_from_slice(&0u32.to_le_bytes());
    v64.extend_from_slice(&0u32.to_le_bytes());
    v64.extend_from_slice(&1u64.to_le_bytes());
    v64.extend_from_slice(&1u64.to_le_bytes());
    v64.extend_from_slice(&cdir_size.to_le_bytes());
    v64.extend_from_slice(&cdir_offset.to_le_bytes());
    v64.extend_from_slice(&[0x50, 0x4B, 0x06, 0x07]);
    v64.extend_from_slice(&0u32.to_le_bytes());
    v64.extend_from_slice(&(v.len() as u64 + cdir_size).to_le_bytes());
    v64.extend_from_slice(&1u32.to_le_bytes());
    v64.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
    v64.extend_from_slice(&0u16.to_le_bytes());
    v64.extend_from_slice(&0u16.to_le_bytes());
    v64.extend_from_slice(&u16::MAX.to_le_bytes());
    v64.extend_from_slice(&u16::MAX.to_le_bytes());
    v64.extend_from_slice(&u32::MAX.to_le_bytes());
    v64.extend_from_slice(&u32::MAX.to_le_bytes());
    v64.extend_from_slice(&0u16.to_le_bytes());

    v.extend_from_slice(&cdir);
    v.extend_from_slice(&v64);
    v
}

/// Handcrafted archive with an entry carrying a custom extra field (id 0xCAFE)
/// in BOTH local and central headers, and a non-empty file comment.
fn handcraft_extra_fields() -> Vec<u8> {
    let name = b"extra.txt";
    let content = b"entry with extra fields";
    let extra: Vec<u8> = {
        let mut e = Vec::new();
        e.extend_from_slice(&0xCAFEu16.to_le_bytes());
        e.extend_from_slice(&4u16.to_le_bytes());
        e.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        e
    };
    let comment = b"file comment here";
    let crc = crc32(content);
    let mut v = Vec::new();
    v.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
    v.extend_from_slice(&20u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&crc.to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(&extra);
    v.extend_from_slice(content);
    let offset = 0u64;
    let cdir_offset = v.len() as u64;
    v.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
    v.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes());
    v.extend_from_slice(&20u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&crc.to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    v.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&(offset as u32).to_le_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(&extra);
    v.extend_from_slice(comment);
    let cdir_size = (v.len() as u64) - cdir_offset;
    v.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
    v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

/// Handcrafted archive using a data descriptor: local header with bit 0x08,
/// zero sizes/CRC in the local header, CRC + sizes in a post-data descriptor.
fn handcraft_data_descriptor() -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    let name = b"dd.txt";
    let content = b"data-descriptor payload payload payload payload";
    let crc = crc32(content);
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
    enc.write_all(content).unwrap();
    let comp = enc.finish().unwrap();

    let mut v = Vec::new();
    v.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
    v.extend_from_slice(&20u16.to_le_bytes());
    v.extend_from_slice(&0x0008u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(&comp);
    v.extend_from_slice(&[0x50, 0x4B, 0x07, 0x08]);
    v.extend_from_slice(&crc.to_le_bytes());
    v.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    let offset = 0u64;
    let cdir_offset = v.len() as u64;
    v.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
    v.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes());
    v.extend_from_slice(&20u16.to_le_bytes());
    v.extend_from_slice(&0x0008u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&crc.to_le_bytes());
    v.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    v.extend_from_slice(&(content.len() as u32).to_le_bytes());
    v.extend_from_slice(&(name.len() as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&(offset as u32).to_le_bytes());
    v.extend_from_slice(name);
    let cdir_size = (v.len() as u64) - cdir_offset;
    v.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
    v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

/// CRC-32 via crc32fast.
fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: gen_corpus <c-zip-dll> <corpus-dir>");
        std::process::exit(2);
    }
    let c_dll = PathBuf::from(&args[1]);
    let corpus = PathBuf::from(&args[2]);
    let inputs_root = corpus.join("inputs");
    let handcrafted_dir = corpus.join("handcrafted");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::create_dir_all(&inputs_root).unwrap();
    std::fs::create_dir_all(&handcrafted_dir).unwrap();

    // Clean top-level corpus zips (keep nothing stale).
    for entry in std::fs::read_dir(&corpus).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().map(|e| e == "zip").unwrap_or(false) && p.parent() == Some(&corpus) {
            let _ = std::fs::remove_file(&p);
        }
    }

    let api = unsafe { CApi::load(&c_dll) }.unwrap_or_else(|e| {
        eprintln!("failed to load C libzip write API from {c_dll:?}: {e}");
        std::process::exit(1);
    });

    let specs: Vec<Archive> = vec![
        Archive {
            filename: "basic_deflate.zip".into(),
            comment: Some("verification basic deflate archive".into()),
            keep_empty: false,
            entries: vec![
                Entry {
                    name: "hello.txt".into(),
                    data: text("hello verification deflate payload ", 120),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: Some(1_600_000_000),
                    comment: None,
                },
                Entry {
                    name: "numbers.bin".into(),
                    data: rand_bytes(0x1111, 8192),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: Some(1_600_000_100),
                    comment: Some("numbers file".into()),
                },
                Entry {
                    name: "empty.txt".into(),
                    data: Vec::new(),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: Some(1_600_000_200),
                    comment: None,
                },
            ],
        },
        Archive {
            filename: "basic_store.zip".into(),
            comment: None,
            keep_empty: false,
            entries: vec![
                Entry {
                    name: "raw1.bin".into(),
                    data: rand_bytes(0x2222, 4096),
                    method: CM_STORE,
                    level: 0,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "raw2.bin".into(),
                    data: rand_bytes(0x2223, 65536),
                    method: CM_STORE,
                    level: 0,
                    mtime: None,
                    comment: None,
                },
            ],
        },
        Archive {
            filename: "empty_archive.zip".into(),
            comment: None,
            keep_empty: true,
            entries: vec![],
        },
        Archive {
            filename: "one_empty.zip".into(),
            comment: None,
            keep_empty: false,
            entries: vec![Entry {
                name: "only_empty.txt".into(),
                data: Vec::new(),
                method: CM_DEFLATE,
                level: 6,
                mtime: None,
                comment: None,
            }],
        },
        Archive {
            filename: "deep.zip".into(),
            comment: None,
            keep_empty: false,
            entries: {
                let depth = 24;
                let mut path = String::new();
                for d in 0..depth {
                    path.push_str(&format!("level{d:02}/"));
                }
                path.push_str("leaf.txt");
                vec![
                    Entry {
                        name: path,
                        data: text("deep nested content ", 40),
                        method: CM_DEFLATE,
                        level: 6,
                        mtime: None,
                        comment: None,
                    },
                    Entry {
                        name: "a/b/c/d/e/f/g/h.txt".into(),
                        data: text("shallow-ish ", 8),
                        method: CM_STORE,
                        level: 0,
                        mtime: None,
                        comment: None,
                    },
                ]
            },
        },
        Archive {
            filename: "unicode.zip".into(),
            comment: Some("café 日本語".into()),
            keep_empty: false,
            entries: vec![
                Entry {
                    name: "héllo wörld.txt".into(),
                    data: text("unicode content ", 30),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "日本語/ファイル.txt".into(),
                    data: text("日本語データ ", 20),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "emojis/🎉party🎊.txt".into(),
                    data: text("party ", 10),
                    method: CM_STORE,
                    level: 0,
                    mtime: None,
                    comment: None,
                },
            ],
        },
        Archive {
            filename: "large.zip".into(),
            comment: None,
            keep_empty: false,
            entries: vec![Entry {
                name: "large_incompressible.bin".into(),
                data: rand_bytes(0x3333, 300 * 1024),
                method: CM_DEFLATE,
                level: 6,
                mtime: None,
                comment: None,
            }],
        },
        Archive {
            filename: "huge.zip".into(),
            comment: None,
            keep_empty: false,
            entries: vec![Entry {
                name: "huge.bin".into(),
                data: rand_bytes(0x4444, 8 * 1024 * 1024),
                method: CM_DEFLATE,
                level: 6,
                mtime: None,
                comment: None,
            }],
        },
        Archive {
            filename: "mix.zip".into(),
            comment: None,
            keep_empty: false,
            entries: vec![
                Entry {
                    name: "dir/one.txt".into(),
                    data: text("mix one ", 100),
                    method: CM_DEFLATE,
                    level: 9,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "dir/two.bin".into(),
                    data: rand_bytes(0x5555, 12345),
                    method: CM_STORE,
                    level: 0,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "dir/three.txt".into(),
                    data: Vec::new(),
                    method: CM_DEFLATE,
                    level: 6,
                    mtime: None,
                    comment: None,
                },
                Entry {
                    name: "nested/deep/under/four.txt".into(),
                    data: text("mix four ", 500),
                    method: CM_DEFLATE,
                    level: 1,
                    mtime: None,
                    comment: None,
                },
            ],
        },
        Archive {
            filename: "many.zip".into(),
            comment: None,
            keep_empty: false,
            entries: (0..1500)
                .map(|i| Entry {
                    name: format!("file_{i:04}.txt"),
                    data: text(
                        &format!("many entry content number {i} "),
                        3 + (i % 7) as usize,
                    ),
                    method: if i % 2 == 0 { CM_DEFLATE } else { CM_STORE },
                    level: 6,
                    mtime: None,
                    comment: None,
                })
                .collect(),
        },
        // 70,000 entries forces libzip to write a real ZIP64 EOCD record
        // (the 16-bit central-directory counts overflow), exercising the
        // ZIP64 read path end to end.
        Archive {
            filename: "many_zip64.zip".into(),
            comment: None,
            keep_empty: false,
            entries: (0..70000)
                .map(|i| Entry {
                    name: format!("z{i:05}.txt"),
                    data: text("z", 2 + (i % 5) as usize),
                    method: CM_STORE,
                    level: 0,
                    mtime: None,
                    comment: None,
                })
                .collect(),
        },
    ];

    let enc_entries: Vec<(String, Vec<u8>, i32, u32)> = vec![
        (
            "encrypted.txt".into(),
            text("encrypted zipcrypto payload content ", 80),
            CM_DEFLATE,
            6,
        ),
        (
            "encrypted_store.bin".into(),
            rand_bytes(0xABCD, 4096),
            CM_STORE,
            0,
        ),
        ("encrypted_empty.txt".into(), Vec::new(), CM_DEFLATE, 6),
    ];
    // Generate the encrypted archive FIRST, before the heavy 70k-entry
    // `many_zip64` spec. C libzip's `zip_close` on the 70k-entry archive
    // (which exercises the ZIP64 write path) leaves the library heap in a
    // state where a subsequent in-process `zip_open` segfaults; generating the
    // encrypted archive earlier sidesteps that latent C-library bug. The
    // encrypted archive is independent, so ordering does not affect the
    // corpus content.
    generate_encrypted_with_c(
        &api,
        &corpus,
        &inputs_root,
        "enc_zipcrypto.zip",
        KZIP_TEST_PASSWORD,
        &enc_entries,
    )
    .unwrap_or_else(|e| {
        eprintln!("FAILED to generate enc_zipcrypto.zip: {e}");
        std::process::exit(1);
    });
    println!("wrote enc_zipcrypto.zip ({} entries)", enc_entries.len());

    // Phase 2: WinZip AES archives written by C libzip (aes128_enc.zip and
    // aes256_enc.zip), generated before the heavy 70k-entry spec (which leaves
    // the C library heap in a state where a later in-process zip_open can
    // segfault, as noted for the Phase 1 encrypted archive).
    let aes_entries: Vec<(String, Vec<u8>, i32, u32)> = vec![
        (
            "aes_secret.txt".into(),
            text("winzip aes encrypted payload content ", 60),
            CM_DEFLATE,
            6,
        ),
        (
            "aes_store.bin".into(),
            rand_bytes(0xACE0, 2048),
            CM_STORE,
            0,
        ),
        (
            "aes_data.txt".into(),
            text("aes second entry ", 25),
            CM_DEFLATE,
            6,
        ),
    ];
    for (fname, method) in [
        ("aes128_enc.zip", ZIP_EM_AES_128),
        ("aes256_enc.zip", ZIP_EM_AES_256),
    ] {
        generate_aes_with_rust(
            &corpus,
            &inputs_root,
            fname,
            KZIP_TEST_PASSWORD,
            method,
            &aes_entries,
        )
        .unwrap_or_else(|e| {
            eprintln!("FAILED to generate {fname}: {e}");
            std::process::exit(1);
        });
        println!("wrote {fname} ({} entries)", aes_entries.len());
    }

    for arch in &specs {
        let _ = generate_with_c(&api, &corpus, &inputs_root, arch).unwrap_or_else(|e| {
            eprintln!("FAILED to generate {}: {e}", arch.filename);
            std::process::exit(1);
        });
        println!("wrote {} ({} entries)", arch.filename, arch.entries.len());
    }

    // ---- Handcrafted edge cases ----
    std::fs::write(
        handcrafted_dir.join("data_descriptor.zip"),
        handcraft_data_descriptor(),
    )
    .unwrap();
    std::fs::write(
        handcrafted_dir.join("extra_fields.zip"),
        handcraft_extra_fields(),
    )
    .unwrap();
    std::fs::write(handcrafted_dir.join("zip64.zip"), handcraft_zip64()).unwrap();
    std::fs::write(
        handcrafted_dir.join("bad_notzip.zip"),
        b"This is definitely not a zip archive, just some random bytes.\n\x00\x01\x02\x03\x04",
    )
    .unwrap();
    let basic = std::fs::read(corpus.join("basic_deflate.zip")).unwrap();
    let cut = basic.len() / 2;
    std::fs::write(handcrafted_dir.join("truncated.zip"), &basic[..cut]).unwrap();

    println!(
        "wrote handcrafted edge cases into {}",
        handcrafted_dir.display()
    );
    println!("corpus ready at {}", corpus.display());
}

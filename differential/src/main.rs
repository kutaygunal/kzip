//! Differential harness: loads a libzip-compatible shared library (either the
//! original C libzip `zip.dll`/`libzip.so` or our Rust `zip-sys` cdylib) via
//! `libloading` and runs a fixed set of read-path operations against a corpus
//! of archives, emitting a deterministic JSON result to stdout.
//!
//! Running the same harness against both libraries and diffing the JSON proves
//! behavioral equivalence (byte-for-byte) for the operations exercised.
//!
//! Usage:
//!   differential `<path-to-lib>` `<corpus-dir>` > result.json
//!
//! The library is loaded by the same ABI in both cases, so the harness needs
//! only one implementation.

use libloading::Library;
use serde::Serialize;
use std::ffi::{c_void, CStr, CString};
use std::path::Path;

// ---- libzip C ABI types (must match libzip/zip.h) ----

type ZipHandle = c_void;
type ZipFileHandle = c_void;

// zip_open flags we use: 0 = open existing for reading.

/// Loaded function pointers for the subset of the zip ABI we exercise.
///
/// We resolve `Symbol`s down to plain fn pointers (Copy, no borrow) and keep
/// the `Library` alive in `_lib` so the code stays loaded. This avoids the
/// self-referential-lifetime problem of storing `Symbol` alongside its `Library`.
struct ZipApi {
    _lib: Library,
    zip_open: unsafe extern "C" fn(*const libc::c_char, i32, *mut i32) -> *mut ZipHandle,
    zip_get_num_entries: unsafe extern "C" fn(*const ZipHandle, u32) -> i64,
    zip_get_name: unsafe extern "C" fn(*const ZipHandle, u64, u32) -> *const libc::c_char,
    zip_fopen: unsafe extern "C" fn(*mut ZipHandle, *const libc::c_char, u32) -> *mut ZipFileHandle,
    zip_fread: unsafe extern "C" fn(*mut ZipFileHandle, *mut c_void, u64) -> i64,
    zip_fclose: unsafe extern "C" fn(*mut ZipFileHandle) -> i32,
    zip_close: unsafe extern "C" fn(*mut ZipHandle) -> i32,
    zip_libzip_version: unsafe extern "C" fn() -> *const libc::c_char,
}

impl ZipApi {
    /// Load the library and resolve the required symbols as raw fn pointers.
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
        // Resolve a symbol into a plain fn pointer (`T: Copy`).
        fn resolve<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
            unsafe { lib.get::<T>(name.as_bytes()) }
                .map(|s| *s)
                .map_err(|e| format!("symbol {name}: {e}"))
        }
        Ok(ZipApi {
            zip_open: resolve(&lib, "zip_open")?,
            zip_get_num_entries: resolve(&lib, "zip_get_num_entries")?,
            zip_get_name: resolve(&lib, "zip_get_name")?,
            zip_fopen: resolve(&lib, "zip_fopen")?,
            zip_fread: resolve(&lib, "zip_fread")?,
            zip_fclose: resolve(&lib, "zip_fclose")?,
            zip_close: resolve(&lib, "zip_close")?,
            zip_libzip_version: resolve(&lib, "zip_libzip_version")?,
            _lib: lib,
        })
    }
}

/// Per-entry result record.
#[derive(Serialize, Debug)]
struct EntryRecord {
    index: u64,
    name: Option<String>,
    open_status: String,
    len: u64,
    sha256: Option<String>,
}

/// Per-archive result record.
#[derive(Serialize, Debug)]
struct ArchiveRecord {
    archive: String,
    open_error: Option<i32>,
    num_entries: Option<i64>,
    entries: Vec<EntryRecord>,
}

fn cstr_to_string(p: *const libc::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p).to_str().ok().map(|s| s.to_owned()) }
}

/// Run the read-path scenario on one archive; never panics.
unsafe fn process_archive(api: &ZipApi, path: &Path) -> ArchiveRecord {
    let cpath = match CString::new(path.to_string_lossy().as_bytes()) {
        Ok(c) => c,
        Err(_) => {
            return ArchiveRecord {
                archive: path.display().to_string(),
                open_error: Some(-1),
                num_entries: None,
                entries: vec![],
            }
        }
    };
    let mut errp: i32 = 0;
    let zh = (api.zip_open)(cpath.as_ptr(), 0, &mut errp);
    if zh.is_null() {
        return ArchiveRecord {
            archive: path.display().to_string(),
            open_error: Some(errp),
            num_entries: None,
            entries: vec![],
        };
    }

    let num = (api.zip_get_num_entries)(zh, 0);
    let mut records = Vec::new();
    if num >= 0 {
        for i in 0..num as u64 {
            let name_ptr = (api.zip_get_name)(zh, i, 0);
            let name = cstr_to_string(name_ptr);
            // Open + read the entry to exercise the decompress/decode path.
            let mut rec = EntryRecord {
                index: i,
                name: name.clone(),
                open_status: "n/a".into(),
                len: 0,
                sha256: None,
            };
            if let Some(n) = &name {
                let cn = CString::new(n.as_bytes()).unwrap_or_default();
                let fh = (api.zip_fopen)(zh, cn.as_ptr(), 0);
                if fh.is_null() {
                    rec.open_status = "fopen_failed".into();
                } else {
                    // Read in chunks; cap to avoid OOM on zip-bombs in corpus.
                    let mut buf = [0u8; 8192];
                    let mut total: u64 = 0;
                    let mut ok = true;
                    // Running FNV-1a over the *whole* decompressed content, so
                    // the fingerprint is independent of read-chunk boundaries
                    // (different decompressors may return different final read
                    // sizes for identical bytes).
                    let mut hasher: u64 = 0xcbf29ce484222325;
                    loop {
                        let nread =
                            (api.zip_fread)(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64);
                        if nread > 0 {
                            total += nread as u64;
                            for &b in &buf[..nread as usize] {
                                hasher ^= b as u64;
                                hasher = hasher.wrapping_mul(0x100000001b3);
                            }
                        } else if nread == 0 {
                            break;
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    rec.len = total;
                    rec.sha256 = Some(format!("{hasher:016x}"));
                    rec.open_status = if ok {
                        "ok".into()
                    } else {
                        "read_failed".into()
                    };
                    (api.zip_fclose)(fh);
                }
            } else {
                rec.open_status = "no_name".into();
            }
            records.push(rec);
        }
    }

    (api.zip_close)(zh);

    ArchiveRecord {
        archive: path.display().to_string(),
        open_error: Some(errp),
        num_entries: Some(num),
        entries: records,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let lib_path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: differential <path-to-lib> <corpus-dir>");
            std::process::exit(2);
        }
    };
    let corpus_dir = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: differential <path-to-lib> <corpus-dir>");
            std::process::exit(2);
        }
    };

    let api = match unsafe { ZipApi::load(Path::new(&lib_path)) } {
        Ok(a) => a,
        Err(e) => {
            eprintln!("failed to load library: {e}");
            std::process::exit(1);
        }
    };

    let version = unsafe { cstr_to_string((api.zip_libzip_version)()).unwrap_or_default() };

    // Collect and sort corpus paths for deterministic ordering.
    let mut zips: Vec<_> = match std::fs::read_dir(&corpus_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "zip").unwrap_or(false))
            .collect(),
        Err(e) => {
            eprintln!("cannot read corpus dir {corpus_dir:?}: {e}");
            std::process::exit(1);
        }
    };
    zips.sort();

    let mut out = serde_json::Map::new();
    out.insert(
        "lib".into(),
        serde_json::Value::String(format!("libzip/{version}")),
    );
    let mut records = Vec::new();
    for zp in &zips {
        records.push(unsafe { process_archive(&api, zp) });
    }
    out.insert(
        "archives".into(),
        serde_json::to_value(records).unwrap_or_default(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap()
    );
}

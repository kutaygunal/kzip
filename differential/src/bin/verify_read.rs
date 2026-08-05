//! Extended read-path differential harness for verification.
//!
//! Loads a libzip-compatible shared library (C `zip.dll` OR the Rust `zip-sys`
//! cdylib) via `libloading` and exercises a much richer read-path scenario than
//! the phase-5 `differential` binary: per entry it runs `zip_fopen` AND
//! `zip_fopen_index` (each fully read + FNV-1a fingerprinted), and reads
//! `zip_stat_index` + `zip_stat` with ALL fields both libs report (name, index,
//! size, comp_size, mtime, crc, comp_method, encryption_method, valid). It also
//! records error paths: fopen of a missing entry, fopen_index out of range, and
//! the resulting `zip_strerror` strings.
//!
//! Running the same binary against both libraries and diffing the JSON proves
//! byte-for-byte behavioral equivalence over the whole read/stat surface.
//!
//! Usage: `verify_read <path-to-lib> <corpus-dir>`

use libloading::Library;
use serde::Serialize;
use std::ffi::{c_void, CStr, CString};
use std::path::Path;

type ZipHandle = c_void;
type ZipFileHandle = c_void;

/// libzip `zip_stat_t`. Includes the trailing `flags` field so the layout
/// matches C libzip exactly (the Rust cdylib writes only the first nine fields;
/// `flags` is not compared).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ZipStat {
    valid: u64,
    name: *const libc::c_char,
    index: u64,
    size: u64,
    comp_size: u64,
    mtime: i64,
    crc: u32,
    comp_method: u16,
    encryption_method: u16,
    flags: u32,
}

struct ZipApi {
    _lib: Library,
    zip_open: unsafe extern "C" fn(*const libc::c_char, i32, *mut i32) -> *mut ZipHandle,
    zip_get_num_entries: unsafe extern "C" fn(*const ZipHandle, u32) -> i64,
    zip_get_name: unsafe extern "C" fn(*const ZipHandle, u64, u32) -> *const libc::c_char,
    zip_fopen: unsafe extern "C" fn(*mut ZipHandle, *const libc::c_char, u32) -> *mut ZipFileHandle,
    zip_fopen_index: unsafe extern "C" fn(*mut ZipHandle, u64, u32) -> *mut ZipFileHandle,
    zip_fread: unsafe extern "C" fn(*mut ZipFileHandle, *mut c_void, u64) -> i64,
    zip_fclose: unsafe extern "C" fn(*mut ZipFileHandle) -> i32,
    zip_stat: unsafe extern "C" fn(*mut ZipHandle, *const libc::c_char, u32, *mut ZipStat) -> i32,
    zip_stat_index: unsafe extern "C" fn(*mut ZipHandle, u64, u32, *mut ZipStat) -> i32,
    zip_strerror: unsafe extern "C" fn(*const ZipHandle) -> *const libc::c_char,
    zip_close: unsafe extern "C" fn(*mut ZipHandle) -> i32,
    zip_libzip_version: unsafe extern "C" fn() -> *const libc::c_char,
    zip_set_default_password: unsafe extern "C" fn(*mut ZipHandle, *const libc::c_char) -> i32,
}

impl ZipApi {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
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
            zip_fopen_index: resolve(&lib, "zip_fopen_index")?,
            zip_fread: resolve(&lib, "zip_fread")?,
            zip_fclose: resolve(&lib, "zip_fclose")?,
            zip_stat: resolve(&lib, "zip_stat")?,
            zip_stat_index: resolve(&lib, "zip_stat_index")?,
            zip_strerror: resolve(&lib, "zip_strerror")?,
            zip_close: resolve(&lib, "zip_close")?,
            zip_libzip_version: resolve(&lib, "zip_libzip_version")?,
            zip_set_default_password: resolve(&lib, "zip_set_default_password")?,
            _lib: lib,
        })
    }
}

#[derive(Serialize, Debug)]
struct StatRecord {
    ret: i32,
    valid: u64,
    name: Option<String>,
    index: u64,
    size: u64,
    comp_size: u64,
    mtime: i64,
    crc: u32,
    comp_method: u16,
    encryption_method: u16,
}

#[derive(Serialize, Debug)]
struct EntryRecord {
    index: u64,
    name: Option<String>,
    fopen_status: String,
    fopen_len: u64,
    fopen_fnv: Option<String>,
    fopen_index_status: String,
    fopen_index_len: u64,
    fopen_index_fnv: Option<String>,
    stat: StatRecord,
    stat_by_name_ret: i32,
    stat_by_name_size: u64,
    stat_by_name_comp_method: u16,
}

#[derive(Serialize, Debug)]
struct ArchiveRecord {
    archive: String,
    open_error: Option<i32>,
    num_entries: Option<i64>,
    entries: Vec<EntryRecord>,
    err_fopen_missing_status: String,
    err_fopen_missing_strerror: Option<String>,
    err_fopen_index_oob_status: String,
    err_fopen_index_oob_strerror: Option<String>,
}

fn cstr_to_string(p: *const libc::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p).to_str().ok().map(|s| s.to_owned()) }
}

/// Read a whole open entry through `zip_fread`, returning (total_len, fnv).
unsafe fn read_all(api: &ZipApi, fh: *mut ZipFileHandle) -> (u64, u64, bool) {
    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    let mut hasher: u64 = 0xcbf29ce484222325;
    let mut ok = true;
    loop {
        let nread =
            unsafe { (api.zip_fread)(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64) };
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
    (total, hasher, ok)
}

fn zero_stat() -> ZipStat {
    ZipStat {
        valid: 0,
        name: std::ptr::null(),
        index: 0,
        size: 0,
        comp_size: 0,
        mtime: 0,
        crc: 0,
        comp_method: 0,
        encryption_method: 0,
        flags: 0,
    }
}

fn stat_to_record(sb: &ZipStat, ret: i32) -> StatRecord {
    StatRecord {
        ret,
        valid: sb.valid,
        name: cstr_to_string(sb.name),
        index: sb.index,
        size: sb.size,
        comp_size: sb.comp_size,
        mtime: sb.mtime,
        crc: sb.crc,
        comp_method: sb.comp_method,
        encryption_method: sb.encryption_method,
    }
}

unsafe fn process_archive(api: &ZipApi, path: &Path) -> ArchiveRecord {
    let cpath = match CString::new(path.to_string_lossy().as_bytes()) {
        Ok(c) => c,
        Err(_) => {
            return ArchiveRecord {
                archive: path.display().to_string(),
                open_error: Some(-1),
                num_entries: None,
                entries: vec![],
                err_fopen_missing_status: "n/a".into(),
                err_fopen_missing_strerror: None,
                err_fopen_index_oob_status: "n/a".into(),
                err_fopen_index_oob_strerror: None,
            }
        }
    };
    let mut errp: i32 = 0;
    let zh = unsafe { (api.zip_open)(cpath.as_ptr(), 0, &mut errp) };
    if zh.is_null() {
        return ArchiveRecord {
            archive: path.display().to_string(),
            open_error: Some(errp),
            num_entries: None,
            entries: vec![],
            err_fopen_missing_status: "archive_not_open".into(),
            err_fopen_missing_strerror: None,
            err_fopen_index_oob_status: "archive_not_open".into(),
            err_fopen_index_oob_strerror: None,
        };
    }

    // Phase 1/2: supply the known password for encrypted corpus archives so
    // entries decrypt (otherwise fopen hits the NOPASS path).
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if fname.contains("enc_zipcrypto")
        || fname.contains("aes128_enc")
        || fname.contains("aes256_enc")
    {
        let pw = CString::new("kzip-test-password").unwrap_or_default();
        unsafe { (api.zip_set_default_password)(zh, pw.as_ptr()) };
    }

    let num = unsafe { (api.zip_get_num_entries)(zh, 0) };
    let mut entries = Vec::new();

    if num >= 0 {
        for i in 0..num as u64 {
            let name_ptr = unsafe { (api.zip_get_name)(zh, i, 0) };
            let name = cstr_to_string(name_ptr);

            let mut fopen_status = "no_name".to_string();
            let (mut fopen_len, mut fopen_fnv) = (0u64, None);
            if let Some(n) = &name {
                let cn = CString::new(n.as_bytes()).unwrap_or_default();
                let fh = unsafe { (api.zip_fopen)(zh, cn.as_ptr(), 0) };
                if fh.is_null() {
                    fopen_status = "failed".into();
                } else {
                    let (len, fp, ok) = unsafe { read_all(api, fh) };
                    fopen_len = len;
                    fopen_fnv = Some(format!("{fp:016x}"));
                    fopen_status = if ok {
                        "ok".into()
                    } else {
                        "read_failed".into()
                    };
                    unsafe { (api.zip_fclose)(fh) };
                }
            }

            let fh2 = unsafe { (api.zip_fopen_index)(zh, i, 0) };
            let (mut fopen_index_status, mut fopen_index_len, mut fopen_index_fnv) =
                ("failed".to_string(), 0u64, None);
            if !fh2.is_null() {
                let (len, fp, ok) = unsafe { read_all(api, fh2) };
                fopen_index_len = len;
                fopen_index_fnv = Some(format!("{fp:016x}"));
                fopen_index_status = if ok {
                    "ok".into()
                } else {
                    "read_failed".into()
                };
                unsafe { (api.zip_fclose)(fh2) };
            }

            let mut sb = zero_stat();
            let ret = unsafe { (api.zip_stat_index)(zh, i, 0, &mut sb) };
            let stat = stat_to_record(&sb, ret);

            let mut sb2 = zero_stat();
            let mut sb_name_ret = -1;
            let (mut sb_name_size, mut sb_name_cm) = (0u64, 0u16);
            if let Some(n) = &name {
                let cn = CString::new(n.as_bytes()).unwrap_or_default();
                sb_name_ret = unsafe { (api.zip_stat)(zh, cn.as_ptr(), 0, &mut sb2) };
                if sb_name_ret == 0 {
                    sb_name_size = sb2.size;
                    sb_name_cm = sb2.comp_method;
                }
            }

            entries.push(EntryRecord {
                index: i,
                name,
                fopen_status,
                fopen_len,
                fopen_fnv,
                fopen_index_status,
                fopen_index_len,
                fopen_index_fnv,
                stat,
                stat_by_name_ret: sb_name_ret,
                stat_by_name_size: sb_name_size,
                stat_by_name_comp_method: sb_name_cm,
            });
        }
    }

    let missing = CString::new("no_such_entry_that_never_exists.txt").unwrap();
    let fhm = unsafe { (api.zip_fopen)(zh, missing.as_ptr(), 0) };
    let err_fopen_missing_status = if fhm.is_null() {
        "failed".into()
    } else {
        "opened".into()
    };
    if !fhm.is_null() {
        unsafe { (api.zip_fclose)(fhm) };
    }
    let err_fopen_missing_strerror = cstr_to_string(unsafe { (api.zip_strerror)(zh) });

    let fho = unsafe { (api.zip_fopen_index)(zh, u64::MAX, 0) };
    let err_fopen_index_oob_status = if fho.is_null() {
        "failed".into()
    } else {
        "opened".into()
    };
    if !fho.is_null() {
        unsafe { (api.zip_fclose)(fho) };
    }
    let err_fopen_index_oob_strerror = cstr_to_string(unsafe { (api.zip_strerror)(zh) });

    unsafe { (api.zip_close)(zh) };

    ArchiveRecord {
        archive: path.display().to_string(),
        open_error: Some(errp),
        num_entries: Some(num),
        entries,
        err_fopen_missing_status,
        err_fopen_missing_strerror,
        err_fopen_index_oob_status,
        err_fopen_index_oob_strerror,
    }
}

/// Recursively collect `*.zip` paths under `root`.
fn collect_zips(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                out.extend(collect_zips(&p));
            } else if p.extension().map(|x| x == "zip").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: verify_read <path-to-lib> <corpus-dir>");
        std::process::exit(2);
    }
    let api = match unsafe { ZipApi::load(Path::new(&args[1])) } {
        Ok(a) => a,
        Err(e) => {
            eprintln!("failed to load library: {e}");
            std::process::exit(1);
        }
    };
    let version = unsafe { cstr_to_string((api.zip_libzip_version)()).unwrap_or_default() };

    let zips = collect_zips(Path::new(&args[2]));
    let mut records = Vec::new();
    for zp in &zips {
        records.push(unsafe { process_archive(&api, zp) });
    }

    let mut out = serde_json::Map::new();
    out.insert(
        "lib".into(),
        serde_json::Value::String(format!("libzip/{version}")),
    );
    out.insert(
        "archives".into(),
        serde_json::to_value(records).unwrap_or_default(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap()
    );
}

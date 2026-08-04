//! # zip-sys
//!
//! C ABI FFI layer that exports `zip-core` behind a libzip-compatible `zip_*`
//! symbol surface (`#[no_mangle]`), so existing C consumers can relink without
//! changes. A generated `zip.h` (via `cbindgen`) mirrors libzip's public header.
//!
//! The core engine lives in `zip-core` (safe Rust). This boundary crate only
//! adapts it to the C ABI: opaque handles, `Mutex` at the ABI boundary for
//! handle-lifetime safety, and `catch_unwind` so a Rust panic returns a ZIP
//! error instead of aborting the host process.
//!
//! # Safety
//!
//! `unsafe` is confined to this crate (the FFI boundary). It is exercised only
//! by C callers that follow the libzip ownership rules. All exported functions
//! are documented with the pointer contract they expect.
//!
//! # Implemented subset (Phase 4)
//!
//! COMPLETE read-path symbols:
//! - `zip_open`, `zip_close`, `zip_get_num_entries`, `zip_get_name`
//! - `zip_fopen`, `zip_fopen_index`, `zip_fread`, `zip_fclose`
//! - `zip_strerror`, `zip_file_strerror`
//! - `zip_stat`, `zip_stat_index`, `zip_stat_init`
//! - `zip_libzip_version`
//!
//! STUBBED / DEFERRED (see `docs/ABI.md` for the full 139-symbol tracking): the
//! write/edit path, encryption, progress/cancel, and source-construction APIs.
#![allow(unsafe_code)]

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use zip_core::{Archive, EntryReader, Stat, ZipErrorCode};

/// `zip_stat_t` layout, mirroring libzip's `zip_stat` struct
/// (`valid`,`name`,`index`,`size`,`comp_size`,`mtime`,`crc`,`comp_method`,
/// `encryption_method`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct zip_stat {
    pub valid: u64,
    pub name: *const c_char,
    pub index: u64,
    pub size: u64,
    pub comp_size: u64,
    pub mtime: i64,
    pub crc: u32,
    pub comp_method: u16,
    pub encryption_method: u16,
}

/// The Rust state behind an opaque `zip_t*`.
///
/// All mutable fields are interior-mutable (`AtomicI32` / `Mutex`) so that the
/// handle can be shared across threads via a **shared** `&Zip` reference
/// (recovered with `as_ref()`) without ever forming an aliasing `&mut` from the
/// raw handle pointer. Concurrent access to one handle is serialized by these
/// mutexes and is data-race-free / alias-rule-sound.
struct Zip {
    archive: Archive,
    /// Names as owned C strings (index-aligned with `archive`), so
    /// `zip_get_name`/`zip_stat` can return stable pointers valid until close.
    names: Vec<Option<CString>>,
    last_error: AtomicI32,
    err_msg: Mutex<CString>,
}

/// The Rust state behind an opaque `zip_file_t*` (an open entry reader).
///
/// Same interior-mutability design as [`Zip`]: the reader is guarded by a
/// `Mutex` and errors are stored atomically, so `&ZipFile` (via `as_ref()`)
/// can be shared across threads safely.
struct ZipFile {
    reader: Mutex<EntryReader>,
    last_error: AtomicI32,
    err_msg: Mutex<CString>,
}

impl Zip {
    fn set_err(&self, code: i32) {
        self.last_error.store(code, Ordering::Relaxed);
        *self.err_msg.lock().unwrap() = CString::new(err_str(code)).unwrap_or_default();
    }
}

impl ZipFile {
    fn set_err(&self, code: i32) {
        self.last_error.store(code, Ordering::Relaxed);
        *self.err_msg.lock().unwrap() = CString::new(err_str(code)).unwrap_or_default();
    }
}

/// Map a zip error code to a short, libzip-style message string.
fn err_str(code: i32) -> &'static str {
    match ZipErrorCode::from_i32(code) {
        ZipErrorCode::Ok => "no error",
        ZipErrorCode::Noent => "no such file",
        ZipErrorCode::Exists => "file already exists",
        ZipErrorCode::Open => "can't open file",
        ZipErrorCode::Read => "read error",
        ZipErrorCode::Write => "write error",
        ZipErrorCode::Seek => "seek error",
        ZipErrorCode::Inval => "invalid argument",
        ZipErrorCode::Nozip => "not a zip archive",
        ZipErrorCode::Compnotsupp => "compression method not supported",
        ZipErrorCode::Encrmethnotsupp => "encryption method not supported",
        ZipErrorCode::Memory => "out of memory",
        ZipErrorCode::Internal => "internal error",
        _ => "zip error",
    }
}

/// Run a fallible closure and turn any panic into a ZIP error sentinel instead
/// of aborting the host process. `on_panic` is returned on a panic or error.
#[inline]
fn guarded<T>(f: impl FnOnce() -> Result<T, i32>, on_panic: T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => on_panic,
        Err(_) => on_panic,
    }
}

/// Convenience alias for handle pointers we hand back to C.
type H = *mut c_void;

// ---------------------------------------------------------------------------
// Archive lifecycle
// ---------------------------------------------------------------------------

/// Open an archive from a filesystem path.
///
/// `path` must be a NUL-terminated C string. On success returns a non-null
/// opaque handle and writes `0` to `errorp` (if non-null); on failure returns
/// NULL and writes a `ZIP_ER_*` code to `errorp`. The returned handle must be
/// released with [`zip_close`].
///
/// The file is read into an in-memory contiguous buffer and the archive is
/// opened from a buffer-backed source. This makes `duplicate()` (used by every
/// entry open) a pure, thread-safe clone, so the returned handle can be shared
/// across threads via `zip_fopen` without racing an OS file-position pointer.
///
/// # Safety
///
/// `path` must point to a valid C string; `errorp`, if non-null, must point to
/// writable `int` storage for the lifetime of the call.
#[no_mangle]
pub unsafe extern "C" fn zip_open(path: *const c_char, _flags: c_int, errorp: *mut c_int) -> H {
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if path.is_null() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        let cpath = unsafe { CStr::from_ptr(path) };
        let path = cpath.to_str().map_err(|_| ZipErrorCode::Inval.as_i32())?;
        let mut file = std::fs::File::open(path).map_err(|_| ZipErrorCode::Open.as_i32())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| ZipErrorCode::Read.as_i32())?;
        let archive = Archive::open(std::io::Cursor::new(bytes)).map_err(|e| e.code().as_i32())?;
        let names = (0..archive.len())
            .map(|i| archive.name(i).and_then(|n| CString::new(n).ok()))
            .collect::<Vec<_>>();
        let z = Box::new(Zip {
            archive,
            names,
            last_error: AtomicI32::new(0),
            err_msg: Mutex::new(CString::new(err_str(0)).unwrap_or_default()),
        });
        Ok(Box::into_raw(z) as H)
    }));
    match r {
        Ok(Ok(h)) => {
            if !errorp.is_null() {
                unsafe {
                    *errorp = 0;
                }
            }
            h
        }
        Ok(Err(code)) => {
            if !errorp.is_null() {
                unsafe {
                    *errorp = code;
                }
            }
            std::ptr::null_mut()
        }
        Err(_) => {
            if !errorp.is_null() {
                unsafe {
                    *errorp = ZipErrorCode::Internal.as_i32();
                }
            }
            std::ptr::null_mut()
        }
    }
}

/// Release an archive opened by [`zip_open`].
///
/// # Safety
///
/// `zh` must be a handle returned by [`zip_open`] that has not already been
/// closed.
#[no_mangle]
pub unsafe extern "C" fn zip_close(zh: H) -> c_int {
    guarded(
        || {
            if zh.is_null() {
                return Err(ZipErrorCode::Inval.as_i32());
            }
            drop(Box::from_raw(zh.cast::<Zip>()));
            Ok(0)
        },
        -1,
    )
}

/// Number of entries in the archive, or -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle from [`zip_open`].
#[no_mangle]
pub unsafe extern "C" fn zip_get_num_entries(zh: H, _flags: u32) -> i64 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            Ok(z.archive.len() as i64)
        },
        -1,
    )
}

/// Name of the entry at `index`, or NULL if out of range.
///
/// The returned pointer is valid until [`zip_close`]. The caller must not free
/// it.
///
/// # Safety
///
/// `zh` must be a valid, open handle from [`zip_open`].
#[no_mangle]
pub unsafe extern "C" fn zip_get_name(zh: H, index: u64, _flags: u32) -> *const c_char {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let p = z
                .names
                .get(index as usize)
                .and_then(|n| n.as_ref())
                .map(|c| c.as_ptr())
                .unwrap_or(std::ptr::null());
            Ok(p)
        },
        std::ptr::null(),
    )
}

// ---------------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------------

/// Return the error string for the archive, valid until the next
/// `zip_strerror` call on the same handle.
///
/// # Safety
///
/// `zh` must be a valid, open handle from [`zip_open`].
#[no_mangle]
pub unsafe extern "C" fn zip_strerror(zh: H) -> *const c_char {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let p = z
                .err_msg
                .lock()
                .map(|g| g.as_ptr())
                .unwrap_or(std::ptr::null());
            Ok(p)
        },
        std::ptr::null(),
    )
}

/// Return the error string for an open file handle.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`].
#[no_mangle]
pub unsafe extern "C" fn zip_file_strerror(fh: H) -> *const c_char {
    guarded(
        || {
            let f = fh.cast::<ZipFile>().as_ref().ok_or(-1)?;
            let p = f
                .err_msg
                .lock()
                .map(|g| g.as_ptr())
                .unwrap_or(std::ptr::null());
            Ok(p)
        },
        std::ptr::null(),
    )
}

// ---------------------------------------------------------------------------
// Entry reading
// ---------------------------------------------------------------------------

/// Open an entry by name for reading, returning an opaque `zip_file_t*` or NULL.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name` must be a NUL-terminated C string.
/// The returned handle must be released with [`zip_fclose`].
#[no_mangle]
pub unsafe extern "C" fn zip_fopen(zh: H, name: *const c_char, _flags: u32) -> H {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if name.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32());
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s,
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32());
                    return Err(-1);
                }
            };
            match z.archive.open_by_name(name) {
                Ok(reader) => {
                    let f = Box::new(ZipFile {
                        reader: Mutex::new(reader),
                        last_error: AtomicI32::new(0),
                        err_msg: Mutex::new(CString::new(err_str(0)).unwrap_or_default()),
                    });
                    Ok(Box::into_raw(f) as H)
                }
                Err(e) => {
                    z.set_err(e.code().as_i32());
                    Err(-1)
                }
            }
        },
        std::ptr::null_mut(),
    )
}

/// Open an entry by index for reading.
///
/// # Safety
///
/// `zh` must be a valid, open handle. The returned handle must be released with
/// [`zip_fclose`].
#[no_mangle]
pub unsafe extern "C" fn zip_fopen_index(zh: H, index: u64, _flags: u32) -> H {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            match z.archive.open_entry(index) {
                Ok(reader) => {
                    let f = Box::new(ZipFile {
                        reader: Mutex::new(reader),
                        last_error: AtomicI32::new(0),
                        err_msg: Mutex::new(CString::new(err_str(0)).unwrap_or_default()),
                    });
                    Ok(Box::into_raw(f) as H)
                }
                Err(e) => {
                    z.set_err(e.code().as_i32());
                    Err(-1)
                }
            }
        },
        std::ptr::null_mut(),
    )
}

/// Read up to `nbytes` bytes from an open entry. Returns the number of bytes
/// read, 0 at EOF, or -1 on error.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`]; `buf` must point to a
/// writable buffer of at least `nbytes` bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_fread(fh: H, buf: *mut c_void, nbytes: u64) -> i64 {
    guarded(
        || {
            let f = fh.cast::<ZipFile>().as_ref().ok_or(-1)?;
            if buf.is_null() {
                f.set_err(ZipErrorCode::Inval.as_i32());
                return Err(-1);
            }
            let slice =
                unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), nbytes as usize) };
            let n = {
                let mut reader = f.reader.lock().unwrap_or_else(|e| e.into_inner());
                reader.read(slice)
            };
            match n {
                Ok(k) => Ok(k as i64),
                Err(_) => {
                    f.set_err(ZipErrorCode::Read.as_i32());
                    Ok(-1)
                }
            }
        },
        -1,
    )
}

/// Close an open entry handle. Returns 0.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`] not already closed.
#[no_mangle]
pub unsafe extern "C" fn zip_fclose(fh: H) -> c_int {
    guarded(
        || {
            if fh.is_null() {
                return Err(-1);
            }
            drop(Box::from_raw(fh.cast::<ZipFile>()));
            Ok(0)
        },
        -1,
    )
}

// ---------------------------------------------------------------------------
// Stat path
// ---------------------------------------------------------------------------

/// Zero-initialize a `zip_stat_t`.
///
/// # Safety
///
/// `sb` must point to writable `zip_stat` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_stat_init(sb: *mut zip_stat) {
    if !sb.is_null() {
        unsafe {
            (*sb).valid = 0;
            (*sb).name = std::ptr::null();
            (*sb).index = 0;
            (*sb).size = 0;
            (*sb).comp_size = 0;
            (*sb).mtime = 0;
            (*sb).crc = 0;
            (*sb).comp_method = 0;
            (*sb).encryption_method = 0;
        }
    }
}

fn fill_stat(z: &Zip, stat: &Stat, sb: *mut zip_stat) -> c_int {
    if sb.is_null() {
        return -1;
    }
    unsafe {
        zip_stat_init(sb);
        let name_ptr = z
            .names
            .get(stat.index.unwrap_or(0) as usize)
            .and_then(|n| n.as_ref())
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());
        (*sb).valid = stat.valid;
        (*sb).name = name_ptr;
        (*sb).index = stat.index.unwrap_or(0);
        (*sb).size = stat.size.unwrap_or(0);
        (*sb).comp_size = stat.comp_size.unwrap_or(0);
        (*sb).mtime = stat.mtime.unwrap_or(0) as i64;
        (*sb).crc = stat.crc.unwrap_or(0);
        (*sb).comp_method = stat.comp_method.unwrap_or(0);
        (*sb).encryption_method = stat.encryption_method.unwrap_or(0);
    }
    0
}

/// Fill `sb` with stat data for the entry named `fname`. Returns 0 on success,
/// -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `fname` must be a NUL-terminated C
/// string; `sb` must point to writable `zip_stat` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_stat(
    zh: H,
    fname: *const c_char,
    _flags: u32,
    sb: *mut zip_stat,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if fname.is_null() {
                return Err(-1);
            }
            let name = CStr::from_ptr(fname).to_str().map_err(|_| -1)?;
            let idx = z.archive.name_locate(name).ok_or(-1)?;
            let stat = z.archive.stat(idx).map_err(|e| {
                z.set_err(e.code().as_i32());
                -1
            })?;
            Ok(fill_stat(z, &stat, sb))
        },
        -1,
    )
}

/// Fill `sb` with stat data for the entry at `index`. Returns 0 on success,
/// -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `sb` must point to writable `zip_stat`.
#[no_mangle]
pub unsafe extern "C" fn zip_stat_index(
    zh: H,
    index: u64,
    _flags: u32,
    sb: *mut zip_stat,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let stat = z.archive.stat(index).map_err(|e| {
                z.set_err(e.code().as_i32());
                -1
            })?;
            Ok(fill_stat(z, &stat, sb))
        },
        -1,
    )
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

/// Return the libzip-compatible version string. Matches the C baseline so the
/// differential harness `lib` field is byte-identical.
///
/// The returned pointer is static and never freed.
#[no_mangle]
pub extern "C" fn zip_libzip_version() -> *const c_char {
    static V: &[u8] = b"1.11.4\0";
    V.as_ptr() as *const c_char
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip_core::{write_archive, ArchiveFile, CompressOptions};

    /// Build a small zip in a temp file and return its path.
    fn build_zip(path: &std::path::Path) {
        let files = vec![
            ArchiveFile::new("a.txt", b"hello ffi read path content ".repeat(20)),
            ArchiveFile::new("b.bin", vec![0u8; 2048]),
            ArchiveFile::new("empty.txt", Vec::<u8>::new()),
        ];
        let bytes = write_archive(&files, &CompressOptions::default()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn cstr(p: *const c_char) -> String {
        assert!(!p.is_null(), "expected non-null C string");
        unsafe { CStr::from_ptr(p).to_str().unwrap().to_owned() }
    }

    #[test]
    fn ffi_read_path_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zip_sys_ffi_{}.zip", std::process::id()));
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let mut errp: c_int = -1;
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
        assert!(!zh.is_null(), "zip_open failed errp={errp}");
        assert_eq!(errp, 0);

        assert_eq!(unsafe { zip_get_num_entries(zh, 0) }, 3);
        let name = unsafe { cstr(zip_get_name(zh, 1, 0)) };
        assert_eq!(name, "b.bin");

        // fopen by name, read all, compare with expected content.
        let fname = CString::new("b.bin").unwrap();
        let fh = unsafe { zip_fopen(zh, fname.as_ptr(), 0) };
        assert!(!fh.is_null(), "zip_fopen failed");
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64) };
            assert!(n >= 0, "zip_fread error");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        assert_eq!(out.len(), 2048);
        assert_eq!(out, vec![0u8; 2048]);
        assert_eq!(unsafe { zip_fclose(fh) }, 0);

        // fopen_index path.
        let fh2 = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(!fh2.is_null());
        let mut buf = [0u8; 32];
        let n = unsafe { zip_fread(fh2, buf.as_mut_ptr() as *mut c_void, buf.len() as u64) };
        assert!(n > 0);
        assert_eq!(unsafe { zip_fclose(fh2) }, 0);

        // stat path.
        let mut sb: zip_stat = zip_stat {
            valid: 0,
            name: std::ptr::null(),
            index: 0,
            size: 0,
            comp_size: 0,
            mtime: 0,
            crc: 0,
            comp_method: 0,
            encryption_method: 0,
        };
        assert_eq!(
            unsafe { zip_stat(zh, CString::new("b.bin").unwrap().as_ptr(), 0, &mut sb) },
            0
        );
        assert_eq!(sb.size, 2048);
        assert_eq!(sb.index, 1);

        // missing entry -> NULL handle.
        let missing = CString::new("nope.txt").unwrap();
        let fhm = unsafe { zip_fopen(zh, missing.as_ptr(), 0) };
        assert!(fhm.is_null());

        assert_eq!(unsafe { zip_close(zh) }, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ffi_open_missing_file_sets_error() {
        let path = std::env::temp_dir().join(format!("zip_sys_missing_{}.zip", std::process::id()));
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut errp: c_int = 0;
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
        assert!(zh.is_null());
        assert_ne!(errp, 0);
    }

    #[test]
    fn ffi_version_matches_baseline() {
        let v = cstr(zip_libzip_version());
        assert_eq!(v, "1.11.4");
    }

    /// Unique temp path per test suffix (tests run in parallel in one process).
    fn temp_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("zip_sys_{}_{}.zip", suffix, std::process::id()))
    }

    fn zero_stat() -> zip_stat {
        zip_stat {
            valid: 0,
            name: std::ptr::null(),
            index: 0,
            size: 0,
            comp_size: 0,
            mtime: 0,
            crc: 0,
            comp_method: 0,
            encryption_method: 0,
        }
    }

    /// All exported functions must tolerate a NULL handle (and NULL args)
    /// without aborting or panicking — they return the documented sentinel.
    #[test]
    fn ffi_null_handles_are_safe() {
        assert_eq!(unsafe { zip_close(std::ptr::null_mut()) }, -1);
        assert_eq!(unsafe { zip_get_num_entries(std::ptr::null_mut(), 0) }, -1);
        assert!(unsafe { zip_get_name(std::ptr::null_mut(), 0, 0) }.is_null());
        assert!(unsafe { zip_strerror(std::ptr::null_mut()) }.is_null());
        assert!(unsafe { zip_file_strerror(std::ptr::null_mut()) }.is_null());
        assert_eq!(unsafe { zip_fclose(std::ptr::null_mut()) }, -1);

        let buf = [0u8; 8];
        assert_eq!(
            unsafe { zip_fread(std::ptr::null_mut(), buf.as_ptr() as *mut c_void, 8) },
            -1
        );
        let name = CString::new("a.txt").unwrap();
        assert!(unsafe { zip_fopen(std::ptr::null_mut(), name.as_ptr(), 0) }.is_null());
        assert!(unsafe { zip_fopen_index(std::ptr::null_mut(), 0, 0) }.is_null());
        let mut sb = zero_stat();
        assert_eq!(
            unsafe { zip_stat(std::ptr::null_mut(), name.as_ptr(), 0, &mut sb) },
            -1
        );
        assert_eq!(
            unsafe { zip_stat_index(std::ptr::null_mut(), 0, 0, &mut sb) },
            -1
        );

        // NULL buffer to zip_fread is also an error sentinel, not a panic.
        let path = temp_path("nullbuf");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(!fh.is_null());
        assert_eq!(unsafe { zip_fread(fh, std::ptr::null_mut(), 64) }, -1);
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// fopen with a missing name returns NULL and sets the archive error;
    /// fopen with a NULL name is rejected and reflected via zip_strerror.
    #[test]
    fn ffi_fopen_error_paths_set_strerror() {
        let path = temp_path("err");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());

        // Missing entry -> NULL handle and a "no such file" strerror.
        let missing = CString::new("nope.txt").unwrap();
        assert!(unsafe { zip_fopen(zh, missing.as_ptr(), 0) }.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "no such file");

        // NULL name -> NULL handle, invalid-argument strerror.
        assert!(unsafe { zip_fopen(zh, std::ptr::null(), 0) }.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "invalid argument");

        // fopen_index out of range -> NULL handle.
        assert!(unsafe { zip_fopen_index(zh, 9999, 0) }.is_null());

        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_stat_index`/`zip_stat` populate correct fields; `zip_stat_init`
    /// zeroes the struct.
    #[test]
    fn ffi_stat_fields_correctness() {
        let path = temp_path("stat");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());

        // a.txt = 28 chars * 20 = 560 bytes, deflated.
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh, 0, 0, &mut sb) }, 0);
        assert_eq!(sb.index, 0);
        assert_eq!(sb.size, 560);
        assert!(
            sb.comp_size > 0 && sb.comp_size < 520,
            "deflate should shrink"
        );
        assert_eq!(sb.comp_method, 8); // deflate
        assert_ne!(sb.crc, 0);
        assert_ne!(sb.valid, 0);
        assert_eq!(sb.encryption_method, 0xFFFF); // ZIP_EM_NONE

        // by-name stat on b.bin (2048 zeros).
        let mut sb2 = zero_stat();
        assert_eq!(
            unsafe { zip_stat(zh, CString::new("b.bin").unwrap().as_ptr(), 0, &mut sb2) },
            0
        );
        assert_eq!(sb2.size, 2048);
        assert_eq!(sb2.index, 1);

        // stat by name for a missing entry -> -1.
        assert_eq!(
            unsafe { zip_stat(zh, CString::new("nope").unwrap().as_ptr(), 0, &mut sb2) },
            -1
        );

        // zip_stat_init zeroes every field.
        let mut z = zip_stat {
            valid: 0xFFFF,
            name: zip_libzip_version(),
            index: 5,
            size: 5,
            comp_size: 5,
            mtime: 5,
            crc: 5,
            comp_method: 5,
            encryption_method: 5,
        };
        unsafe { zip_stat_init(&mut z) };
        assert_eq!(z.valid, 0);
        assert!(z.name.is_null());
        assert_eq!(z.index, 0);
        assert_eq!(z.size, 0);
        assert_eq!(z.comp_size, 0);
        assert_eq!(z.mtime, 0);
        assert_eq!(z.crc, 0);
        assert_eq!(z.comp_method, 0);
        assert_eq!(z.encryption_method, 0);

        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// A large entry read through the extern ABI spans many `zip_fread` calls
    /// and must reassemble to the exact original content.
    #[test]
    fn ffi_large_entry_multi_chunk_read() {
        let path = temp_path("large");
        let content = b"large multi-chunk payload for the extern ABI ".repeat(4000); // ~172 KB
        let files = vec![zip_core::ArchiveFile::new("big.txt", content.clone())];
        let bytes = zip_core::write_archive(&files, &zip_core::CompressOptions::default()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(!fh.is_null());

        let mut out = Vec::new();
        let mut buf = [0u8; 37]; // odd buffer to cross many chunk boundaries
        loop {
            let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64) };
            assert!(n >= 0, "zip_fread error");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        assert_eq!(out, content);
        assert_eq!(out.len(), content.len());
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// Independent handles opened across threads must each read the full,
    /// correct content (FFI is usable from multiple threads).
    #[test]
    fn ffi_thread_safety_independent_handles() {
        let path = temp_path("threads");
        let expected = b"thread-safe ffi content ".repeat(50);
        let files = vec![zip_core::ArchiveFile::new("t.txt", expected.clone())];
        let bytes = zip_core::write_archive(&files, &zip_core::CompressOptions::default()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let mut threads = Vec::new();
        for _ in 0..4 {
            let cpath = cpath.clone();
            let expected = expected.clone();
            threads.push(std::thread::spawn(move || {
                let mut errp: c_int = -1;
                let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
                assert!(!zh.is_null(), "zip_open failed errp={errp}");
                let fh = unsafe { zip_fopen_index(zh, 0, 0) };
                assert!(!fh.is_null());
                let mut out = Vec::new();
                let mut buf = [0u8; 64];
                loop {
                    let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 64) };
                    assert!(n >= 0);
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n as usize]);
                }
                assert_eq!(out, expected);
                unsafe { zip_fclose(fh) };
                unsafe { zip_close(zh) };
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    /// Concurrent `zip_fopen`/`zip_fread` on the **same archive handle** from
    /// multiple threads. Each thread opens its own reader on the shared handle
    /// and reads it fully; the shared handle is accessed through `as_ref()` so
    /// this is data-race-free and alias-rule-sound (no `&mut` alias is formed).
    ///
    /// The handle is passed to threads as a `usize` (trivially `Send`) and cast
    /// back to the raw pointer inside the thread; the pointer is only handed to
    /// the extern fns, which recover a shared `&`.
    #[test]
    fn ffi_shared_archive_handle_concurrent_fopen() {
        let path = temp_path("shared_archive");
        let expected: Vec<u8> = vec![0x5Au8; 60_000]; // any corruption is detectable
        let files = vec![zip_core::ArchiveFile::new("pattern.bin", expected.clone())];
        let bytes = zip_core::write_archive(&files, &zip_core::CompressOptions::default()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let name = CString::new("pattern.bin").unwrap();

        let mut errp: c_int = -1;
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
        assert!(!zh.is_null(), "zip_open failed errp={errp}");

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let zh = zh as usize;
                let name = name.clone();
                std::thread::spawn(move || {
                    let zh = zh as H;
                    let fh = unsafe { zip_fopen(zh, name.as_ptr(), 0) };
                    assert!(
                        !fh.is_null(),
                        "concurrent zip_fopen on shared handle failed"
                    );
                    let mut out = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 1024) };
                        assert!(n >= 0);
                        if n == 0 {
                            break;
                        }
                        out.extend_from_slice(&buf[..n as usize]);
                    }
                    unsafe { zip_fclose(fh) };
                    out
                })
            })
            .collect();
        for (i, t) in threads.into_iter().enumerate() {
            assert_eq!(t.join().unwrap(), expected, "thread {i} read wrong content");
        }
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// Concurrent `zip_fread` on the **same file handle** from multiple threads.
    /// The internal `Mutex<EntryReader>` serializes reads, so no bytes are lost
    /// or duplicated and no corruption occurs: every byte read must match the
    /// known pattern and the combined byte count must equal the full length.
    #[test]
    fn ffi_shared_file_handle_concurrent_fread() {
        let path = temp_path("shared_file");
        let content: Vec<u8> = vec![0x3Cu8; 50_000]; // constant pattern; corruption is detectable
        let files = vec![zip_core::ArchiveFile::new("c.bin", content.clone())];
        let bytes = zip_core::write_archive(&files, &zip_core::CompressOptions::default()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let mut errp: c_int = -1;
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
        assert!(!zh.is_null());
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(!fh.is_null(), "zip_fopen_index failed");

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let fh = fh as usize;
                std::thread::spawn(move || {
                    let fh = fh as H;
                    let mut total: u64 = 0;
                    let mut all_ok = true;
                    let mut buf = [0u8; 512];
                    loop {
                        let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 512) };
                        assert!(n >= 0, "zip_fread error on shared handle");
                        if n == 0 {
                            break;
                        }
                        total += n as u64;
                        if buf[..n as usize].iter().any(|&b| b != 0x3C) {
                            all_ok = false;
                        }
                    }
                    (total, all_ok)
                })
            })
            .collect();
        let mut combined: u64 = 0;
        for h in handles {
            let (t, ok) = h.join().unwrap();
            combined += t;
            assert!(ok, "corrupted byte read from shared file handle");
        }
        assert_eq!(
            combined,
            content.len() as u64,
            "shared file handle lost or duplicated bytes"
        );
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }
}

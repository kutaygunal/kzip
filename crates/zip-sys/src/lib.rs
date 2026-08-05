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
//! # Implemented subset
//!
//! COMPLETE read-path symbols:
//! - `zip_open`, `zip_close`, `zip_get_num_entries`, `zip_get_name`
//! - `zip_fopen`, `zip_fopen_index`, `zip_fread`, `zip_fclose`
//! - `zip_strerror`, `zip_file_strerror`, `zip_name_locate`
//! - `zip_stat`, `zip_stat_index`, `zip_stat_init`
//! - `zip_libzip_version`
//!
//! COMPLETE write/edit path (buffer-source subset):
//! - `zip_file_add`, `zip_dir_add`, `zip_delete`, `zip_rename`,
//!   `zip_file_replace`, `zip_discard`, `zip_source_buffer`, `zip_source_free`
//! - `zip_close` write-through semantics; `zip_open` `ZIP_CREATE`/`ZIP_TRUNCATE`/`ZIP_RDONLY`
//!
//! COMPLETE structured error object API: `zip_get_error`, `zip_error_init`,
//! `zip_error_init_with_code`, `zip_error_clear`, `zip_error_set`,
//! `zip_error_strerror`, `zip_error_code_zip`, `zip_error_code_system`,
//! `zip_error_fini`, `zip_error_to_str`, `zip_error_system_type`,
//! `zip_error_get`, `zip_error_set_from_source`.
//!
//! COMPLETE fseek/ftell/seekability: `zip_fseek`, `zip_ftell`,
//! `zip_file_is_seekable`. COMPLETE method queries:
//! `zip_compression_method_supported`, `zip_encryption_method_supported`.
//!
//! COMPLETE comment/extra-field READ: `zip_get_archive_comment`,
//! `zip_file_get_comment`, `zip_file_extra_fields_count`,
//! `zip_file_extra_fields_count_by_id`, `zip_file_extra_field_get`,
//! `zip_file_extra_field_get_by_id`.
//!
//! DEFERRED (see `docs/ABI.md`): encryption, the full `zip_source_*` streaming
//! API, progress/cancel callbacks, comment/extra-field WRITE, `zip_unchange*`,
//! `zip_fdopen`/`zip_open_from_source`, Win32 sources.
#![allow(unsafe_code)]

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use zip_core::{
    write_archive, write_archive_encrypted_methods, Archive, ArchiveFile, CompressOptions,
    EntryReader, Stat, ZipErrorCode,
};

// ---------------------------------------------------------------------------
// libzip constants (from libzip/lib/zip.h) needed by the exported ABI.
// ---------------------------------------------------------------------------

/// `zip_open` flags.
const ZIP_CREATE: c_int = 1;
const ZIP_TRUNCATE: c_int = 8;
const ZIP_RDONLY: c_int = 16;

/// `zip_file_add` flag: overwrite an existing entry with the same name.
const ZIP_FL_OVERWRITE: u32 = 8192;

/// `zip_error_system_type` return values.
const ZIP_ET_NONE: c_int = 0;
const ZIP_ET_SYS: c_int = 1;

/// `zip_fseek` whence values (POSIX).
const SEEK_SET: c_int = 0;
const SEEK_CUR: c_int = 1;
const SEEK_END: c_int = 2;

/// Compression method values.
const ZIP_CM_DEFAULT: i32 = -1;
const ZIP_CM_STORE: i32 = 0;
const ZIP_CM_DEFLATE: i32 = 8;
const ZIP_CM_BZIP2: i32 = 12;

/// Encryption method values.
const ZIP_EM_NONE: u16 = 0;
const ZIP_EM_TRAD_PKWARE: u16 = 1;
const ZIP_EM_AES_128: u16 = 0x0101;
const ZIP_EM_AES_192: u16 = 0x0102;
const ZIP_EM_AES_256: u16 = 0x0103;

/// `zip_error_t` layout, mirroring libzip's `struct zip_error`
/// (`zip_err`, `sys_err`, `str`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct zip_error {
    pub zip_err: c_int,
    pub sys_err: c_int,
    pub str: *mut c_char,
}

/// The archive's error object plus ownership of its `str` buffer.
///
/// All access is serialized by the enclosing `Mutex`, so the raw `str` pointer
/// is never touched outside the lock; the manual `Send`/`Sync` impls are
/// therefore sound.
struct ZipErrorState {
    ze: zip_error,
    owned: Option<CString>,
}
unsafe impl Send for ZipErrorState {}
unsafe impl Sync for ZipErrorState {}

/// A pending write operation on an archive, materialized on `zip_close`.
#[derive(Default)]
struct PendingOps {
    /// Entries to append (name + data).
    adds: Vec<PendingAdd>,
    /// Original entry indices to delete.
    deletes: Vec<u64>,
    /// Original entry index -> new name.
    renames: Vec<(u64, String)>,
    /// Original entry index -> replacement data.
    replaces: Vec<(u64, Vec<u8>)>,
    /// Original entry index -> encryption method (0 = none, 1 = ZipCrypto).
    encryptions: Vec<(u64, u16)>,
}

struct PendingAdd {
    name: String,
    data: Vec<u8>,
}

impl PendingOps {
    fn is_empty(&self) -> bool {
        self.adds.is_empty()
            && self.deletes.is_empty()
            && self.renames.is_empty()
            && self.replaces.is_empty()
            && self.encryptions.is_empty()
    }
}

/// The Rust state behind an opaque `zip_source_t*` (a buffer source).
///
/// This is the minimal source model needed to drive the write/edit path
/// (`zip_file_add`/`zip_file_replace`). The full `zip_source_*` streaming API
/// (file/function/layered/window/user-defined) remains deferred.
struct ZipSource {
    data: Vec<u8>,
}

/// `zip_stat_t` layout, mirroring libzip's `zip_stat` struct
/// (`valid`,`name`,`index`,`size`,`comp_size`,`mtime`,`crc`,`comp_method`,
/// `encryption_method`,`flags`). The trailing `flags` field makes the Rust
/// struct 60 bytes, matching the C layout for drop-in ABI compatibility.
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
    pub flags: u32,
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
    /// Per-entry comments as owned C strings (index-aligned with `archive`).
    comments: Vec<Option<CString>>,
    /// Archive (EOCD) comment as an owned C string.
    archive_comment: Option<CString>,
    /// Structured error object (the `zip_error_t` returned by `zip_get_error`).
    error: Mutex<ZipErrorState>,
    // ---- write/edit state (materialized on `zip_close`) ----
    /// Path to write the archive to on close (the path passed to `zip_open`).
    path: Option<CString>,
    /// Flags passed to `zip_open` (ZIP_CREATE / ZIP_TRUNCATE / ...).
    flags: c_int,
    /// Whether the file existed when the archive was opened.
    existed: bool,
    /// Pending write operations.
    pending: Mutex<PendingOps>,
    /// Default password for encrypted entries (set via `zip_set_default_password`).
    password: Mutex<Option<Vec<u8>>>,
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
    fn set_err(&self, code: i32, sys: i32) {
        let mut g = self.error.lock().unwrap_or_else(|e| e.into_inner());
        g.ze.zip_err = code;
        g.ze.sys_err = sys;
        let s = CString::new(err_str(code)).unwrap_or_default();
        g.ze.str = s.as_ptr() as *mut c_char;
        g.owned = Some(s);
    }

    /// Reject a write operation on a read-only archive (`ZIP_RDONLY`).
    fn check_writable(&self) -> Result<(), i32> {
        if self.flags & ZIP_RDONLY != 0 {
            self.set_err(ZipErrorCode::Rdonly.as_i32(), 0);
            return Err(-1);
        }
        Ok(())
    }
}

impl ZipFile {
    fn set_err(&self, code: i32) {
        self.last_error.store(code, Ordering::Relaxed);
        *self.err_msg.lock().unwrap() = CString::new(err_str(code)).unwrap_or_default();
    }
}

/// Map a zip error code to the EXACT message string libzip returns for it
/// (byte-for-byte from libzip's `_zip_err_str[]` in `zip_err_str.c`, indexed by
/// error code). Sentence-case and wording must match the C baseline so the
/// differential harness's `zip_strerror` output is identical.
///
/// Matching on the raw integer (like libzip's table) keeps this self-contained
/// and independent of the `ZipErrorCode` enum, so it stays correct even as
/// new codes are added. Codes 37/38 are zip-bomb guards added by the Rust port
/// (not present in libzip); they get clear, sentence-case messages.
fn err_str(code: i32) -> &'static str {
    match code {
        0 => "No error",
        1 => "Multi-disk zip archives not supported",
        2 => "Renaming temporary file failed",
        3 => "Closing zip archive failed",
        4 => "Seek error",
        5 => "Read error",
        6 => "Write error",
        7 => "CRC error",
        8 => "Containing zip archive was closed",
        9 => "No such file",
        10 => "File already exists",
        11 => "Can't open file",
        12 => "Failure to create temporary file",
        13 => "Zlib error",
        14 => "Malloc failure",
        15 => "Entry has been changed",
        16 => "Compression method not supported",
        17 => "Premature end of file",
        18 => "Invalid argument",
        19 => "Not a zip archive",
        20 => "Internal error",
        21 => "Zip archive inconsistent",
        22 => "Can't remove file",
        23 => "Entry has been deleted",
        24 => "Encryption method not supported",
        25 => "Read-only archive",
        26 => "No password provided",
        27 => "Wrong password provided",
        28 => "Operation not supported",
        29 => "Resource still in use",
        30 => "Tell error",
        31 => "Compressed data invalid",
        32 => "Operation cancelled",
        33 => "Unexpected length of data",
        34 => "Not allowed in torrentzip",
        35 => "Possibly truncated or corrupted zip archive",
        36 => "Extra fields too large",
        37 => "Decompressed data exceeds the size limit",
        38 => "Central directory too large",
        _ => "Internal error",
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

/// Upper bound (bytes) on the whole-file read performed by [`zip_open`].
///
/// `zip_open` reads the entire archive into an in-memory contiguous buffer so
/// the handle can be shared across threads without racing an OS file-position
/// pointer. This bound prevents a huge or malicious file from triggering an
/// unbounded allocation; a file larger than this is rejected with a clear
/// error instead of being read. 2 GiB comfortably covers every archive in the
/// differential corpus (largest is ~8 MiB) and any realistic zip, while still
/// bounding the allocation.
pub const MAX_OPEN_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

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
pub unsafe extern "C" fn zip_open(path: *const c_char, flags: c_int, errorp: *mut c_int) -> H {
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if path.is_null() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        let cpath = unsafe { CStr::from_ptr(path) };
        let path = cpath.to_str().map_err(|_| ZipErrorCode::Inval.as_i32())?;
        let existed = std::path::Path::new(path).exists();
        // Opening a non-existent file requires ZIP_CREATE.
        if !existed && flags & ZIP_CREATE == 0 {
            return Err(ZipErrorCode::Open.as_i32());
        }
        // ZIP_TRUNCATE discards any existing content and starts fresh.
        let truncate = flags & ZIP_TRUNCATE != 0;
        let archive = if existed && !truncate {
            let file = std::fs::File::open(path).map_err(|_| ZipErrorCode::Open.as_i32())?;
            // Bound the whole-file read so a huge/malicious file cannot cause an
            // unbounded allocation. Read at most MAX_OPEN_FILE_SIZE+1 bytes; if we
            // got more than the cap, reject the archive with a clear error.
            let mut bytes = Vec::new();
            file.take(MAX_OPEN_FILE_SIZE + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ZipErrorCode::Read.as_i32())?;
            if bytes.len() as u64 > MAX_OPEN_FILE_SIZE {
                return Err(ZipErrorCode::Memory.as_i32());
            }
            Archive::open(std::io::Cursor::new(bytes)).map_err(|e| e.code().as_i32())?
        } else {
            // New archive: start from an empty (valid) archive.
            let empty =
                write_archive(&[], &CompressOptions::default()).map_err(|e| e.code().as_i32())?;
            Archive::open(std::io::Cursor::new(empty)).map_err(|e| e.code().as_i32())?
        };
        let names = (0..archive.len())
            .map(|i| archive.name(i).and_then(|n| CString::new(n).ok()))
            .collect::<Vec<_>>();
        let comments = (0..archive.len())
            .map(|i| {
                archive
                    .dirent(i)
                    .and_then(|d| CString::new(d.comment.as_str()).ok())
            })
            .collect::<Vec<_>>();
        let archive_comment = CString::new(archive.comment()).ok();
        let z = Box::new(Zip {
            archive,
            names,
            comments,
            archive_comment,
            error: Mutex::new(ZipErrorState {
                ze: zip_error {
                    zip_err: 0,
                    sys_err: 0,
                    str: std::ptr::null_mut(),
                },
                owned: Some(CString::new(err_str(0)).unwrap_or_default()),
            }),
            path: Some(CString::new(path).unwrap_or_default()),
            flags,
            existed,
            pending: Mutex::new(PendingOps::default()),
            password: Mutex::new(None),
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
/// If the archive was opened for writing (`ZIP_CREATE`/`ZIP_TRUNCATE`) or has
/// pending write operations, the archive is materialized and written to the
/// path it was opened from before the handle is freed. Otherwise the handle is
/// simply freed.
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
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let should_write = {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                !pending.is_empty() || (!z.existed && z.flags & ZIP_CREATE != 0)
            };
            if should_write {
                let bytes = materialize(z)?;
                let path = z.path.as_ref().ok_or(ZipErrorCode::Inval.as_i32())?;
                let path_str = std::str::from_utf8(path.to_bytes())
                    .map_err(|_| ZipErrorCode::Inval.as_i32())?;
                std::fs::write(path_str, bytes).map_err(|_| ZipErrorCode::Write.as_i32())?;
            }
            drop(Box::from_raw(zh.cast::<Zip>()));
            Ok(0)
        },
        -1,
    )
}

/// Materialize the archive's current state (original entries minus deletes,
/// with renames/replaces applied, plus appended entries) into a fresh ZIP byte
/// stream.
fn materialize(z: &Zip) -> Result<Vec<u8>, i32> {
    let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
    // Logical index -> (file, encryption method). Logical indices match those
    // returned by `zip_file_add` / the original entry indices used by
    // `zip_file_set_encryption`.
    let mut files: Vec<ArchiveFile> = Vec::new();
    let mut methods: Vec<u16> = Vec::new();
    let n = z.archive.len();
    for i in 0..n {
        if pending.deletes.contains(&i) {
            continue;
        }
        let name = pending
            .renames
            .iter()
            .find(|(idx, _)| *idx == i)
            .map(|(_, nm)| nm.clone())
            .unwrap_or_else(|| z.archive.name(i).unwrap_or("").to_string());
        let data = match pending.replaces.iter().find(|(idx, _)| *idx == i) {
            Some((_, d)) => d.clone(),
            None => z.archive.read_entry(i).map_err(|e| e.code().as_i32())?,
        };
        files.push(ArchiveFile::new(name, data));
        let method = pending
            .encryptions
            .iter()
            .find(|(idx, _)| *idx == i)
            .map(|(_, m)| *m)
            .unwrap_or(0);
        methods.push(method);
    }
    for (k, add) in pending.adds.iter().enumerate() {
        let logical = n + k as u64;
        files.push(ArchiveFile::new(add.name.clone(), add.data.clone()));
        let method = pending
            .encryptions
            .iter()
            .find(|(idx, _)| *idx == logical)
            .map(|(_, m)| *m)
            .unwrap_or(0);
        methods.push(method);
    }
    let any_enc = methods.iter().any(|&m| m != 0);
    if any_enc {
        let pw = z
            .password
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(ZipErrorCode::Nopasswd.as_i32())?;
        write_archive_encrypted_methods(&files, &CompressOptions::default(), &pw, &methods)
            .map_err(|e| e.code().as_i32())
    } else {
        write_archive(&files, &CompressOptions::default()).map_err(|e| e.code().as_i32())
    }
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
            let g = z.error.lock().unwrap_or_else(|e| e.into_inner());
            Ok(g.ze.str as *const c_char)
        },
        std::ptr::null(),
    )
}

/// Find the index of the first entry named `name`, or -1 (with the archive
/// error set to `ZIP_ER_NOENT`) if not found.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name` must be a NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn zip_name_locate(zh: H, name: *const c_char, _flags: u32) -> i64 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if name.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s,
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            match z.archive.name_locate(name) {
                Some(idx) => Ok(idx as i64),
                None => {
                    z.set_err(ZipErrorCode::Noent.as_i32(), 0);
                    Err(-1)
                }
            }
        },
        -1,
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
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s,
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            match z.archive.name_locate(name) {
                Some(idx) => {
                    let data = z.archive.read_entry(idx).map_err(|e| {
                        z.set_err(e.code().as_i32(), 0);
                        -1
                    })?;
                    Ok(make_zipfile(z, data, idx))
                }
                None => {
                    z.set_err(ZipErrorCode::Noent.as_i32(), 0);
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
            let data = z.archive.read_entry(index).map_err(|e| {
                z.set_err(e.code().as_i32(), 0);
                -1
            })?;
            Ok(make_zipfile(z, data, index))
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
// Encrypted entry reading
// ---------------------------------------------------------------------------

/// Build an in-memory `ZipFile` handle from already-decrypted `data`.
unsafe fn make_zipfile(z: &Zip, data: Vec<u8>, index: u64) -> H {
    let is_aes = z
        .archive
        .dirent(index)
        .map(|d| {
            matches!(
                d.encryption_method,
                zip_core::constant::encryption::AES_128
                    | zip_core::constant::encryption::AES_192
                    | zip_core::constant::encryption::AES_256
            )
        })
        .unwrap_or(false);
    match z.archive.stat(index) {
        Ok(stat) => {
            let size = stat.size.unwrap_or(0);
            let buffered = if is_aes {
                // WinZip AES AE-2 entries store CRC 0 (invalid); skip the CRC
                // check (integrity was verified by the HMAC).
                EntryReader::from_buffer_skip_crc(data, size, None)
            } else {
                EntryReader::from_buffer(data, stat.crc.unwrap_or(0), size, None)
            };
            let f = Box::new(ZipFile {
                reader: Mutex::new(buffered),
                last_error: AtomicI32::new(0),
                err_msg: Mutex::new(CString::new(err_str(0)).unwrap_or_default()),
            });
            Box::into_raw(f) as H
        }
        Err(e) => {
            z.set_err(e.code().as_i32(), 0);
            std::ptr::null_mut()
        }
    }
}

/// Open an entry by name for reading, decrypting with `password`. Returns an
/// opaque `zip_file_t*` or NULL (with the archive error set on failure).
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name`/`password` NUL-terminated C
/// strings. The returned handle must be released with [`zip_fclose`].
#[no_mangle]
pub unsafe extern "C" fn zip_fopen_encrypted(
    zh: H,
    name: *const c_char,
    _flags: u32,
    password: *const c_char,
) -> H {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if name.is_null() || password.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s,
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            let pw = CStr::from_ptr(password).to_bytes();
            match z.archive.name_locate(name) {
                Some(idx) => {
                    let data = z
                        .archive
                        .read_entry_with_password(idx, Some(pw))
                        .map_err(|e| {
                            z.set_err(e.code().as_i32(), 0);
                            -1
                        })?;
                    Ok(make_zipfile(z, data, idx))
                }
                None => {
                    z.set_err(ZipErrorCode::Noent.as_i32(), 0);
                    Err(-1)
                }
            }
        },
        std::ptr::null_mut(),
    )
}

/// Open an entry by index for reading, decrypting with `password`.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `password` a NUL-terminated C string.
/// The returned handle must be released with [`zip_fclose`].
#[no_mangle]
pub unsafe extern "C" fn zip_fopen_index_encrypted(
    zh: H,
    index: u64,
    _flags: u32,
    password: *const c_char,
) -> H {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if password.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let pw = CStr::from_ptr(password).to_bytes();
            let data = z
                .archive
                .read_entry_with_password(index, Some(pw))
                .map_err(|e| {
                    z.set_err(e.code().as_i32(), 0);
                    -1
                })?;
            Ok(make_zipfile(z, data, index))
        },
        std::ptr::null_mut(),
    )
}

/// Set the default password used to decrypt encrypted entries (and to encrypt
/// entries on write). Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `password` a NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn zip_set_default_password(zh: H, password: *const c_char) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if password.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let pw = CStr::from_ptr(password).to_bytes().to_vec();
            // Store for write-time encryption (materialize) and set the
            // archive's read-time decryption password.
            *z.password.lock().unwrap_or_else(|e| e.into_inner()) = Some(pw.clone());
            z.archive.set_default_password(&pw);
            Ok(0)
        },
        -1,
    )
}

/// Set the encryption method for the entry at `index`, applied on `zip_close`.
///
/// `ZIP_EM_NONE` (0), `ZIP_EM_TRAD_PKWARE` (1), and the three WinZip AES
/// methods (`ZIP_EM_AES_128/192/256`) are supported; any other method returns
/// `ZIP_ER_ENCRNOTSUPP`. Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_set_encryption(zh: H, index: u64, method: u16) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            // A valid index is an existing entry or a pending add.
            let total = {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                z.archive.len() + pending.adds.len() as u64
            };
            if index >= total {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let supported = method == ZIP_EM_NONE
                || method == ZIP_EM_TRAD_PKWARE
                || method == ZIP_EM_AES_128
                || method == ZIP_EM_AES_192
                || method == ZIP_EM_AES_256;
            if !supported {
                z.set_err(ZipErrorCode::Encrmethnotsupp.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.encryptions.retain(|(idx, _)| *idx != index);
            pending.encryptions.push((index, method));
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
            (*sb).flags = 0;
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
        // `flags` is reserved-for-future-use in libzip's zip_stat_t and stays 0.
        (*sb).flags = 0;
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
                z.set_err(e.code().as_i32(), 0);
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
                z.set_err(e.code().as_i32(), 0);
                -1
            })?;
            Ok(fill_stat(z, &stat, sb))
        },
        -1,
    )
}

// ---------------------------------------------------------------------------
// Write / edit path (C ABI)
// ---------------------------------------------------------------------------

/// Create a buffer-backed `zip_source_t*` from `data[0..len]`.
///
/// The data is copied into an owned buffer (so `freep` is ignored). The
/// returned source must be released with [`zip_source_free`].
///
/// # Safety
///
/// `data` must point to `len` readable bytes (or be NULL when `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn zip_source_buffer(
    _zh: H,
    data: *const c_void,
    len: u64,
    _freep: c_int,
) -> H {
    guarded(
        || {
            if data.is_null() && len > 0 {
                return Err(-1);
            }
            let slice = if len > 0 {
                unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len as usize) }
            } else {
                &[]
            };
            let src = Box::new(ZipSource {
                data: slice.to_vec(),
            });
            Ok(Box::into_raw(src) as H)
        },
        std::ptr::null_mut(),
    )
}

/// Release a source created by [`zip_source_buffer`].
///
/// # Safety
///
/// `source` must be a handle returned by [`zip_source_buffer`] not already
/// freed.
#[no_mangle]
pub unsafe extern "C" fn zip_source_free(source: H) {
    if !source.is_null() {
        drop(Box::from_raw(source.cast::<ZipSource>()));
    }
}

/// Add a new entry to the archive from `source`, returning its index or -1.
///
/// If an entry with the same name already exists and `flags` has
/// `ZIP_FL_OVERWRITE`, the existing entry is replaced; otherwise the archive
/// error is set to `ZIP_ER_EXISTS` and -1 is returned. The change is applied
/// when the archive is closed with [`zip_close`].
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name` a NUL-terminated C string;
/// `source` a valid source from [`zip_source_buffer`].
#[no_mangle]
pub unsafe extern "C" fn zip_file_add(zh: H, name: *const c_char, source: H, flags: u32) -> i64 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if name.is_null() || source.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            let src = source.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let data = src.data.clone();
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(idx) = z.archive.name_locate(&name) {
                if flags & ZIP_FL_OVERWRITE != 0 {
                    pending.replaces.push((idx, data));
                    return Ok(idx as i64);
                }
                z.set_err(ZipErrorCode::Exists.as_i32(), 0);
                return Err(-1);
            }
            let new_index = z.archive.len() + pending.adds.len() as u64;
            pending.adds.push(PendingAdd { name, data });
            Ok(new_index as i64)
        },
        -1,
    )
}

/// Add a directory entry named `name` (a trailing `/` is appended if missing),
/// returning its index or -1.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name` a NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn zip_dir_add(zh: H, name: *const c_char, _flags: u32) -> i64 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if name.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            let dirname = if name.ends_with('/') {
                name
            } else {
                format!("{name}/")
            };
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            if z.archive.name_locate(&dirname).is_some() {
                z.set_err(ZipErrorCode::Exists.as_i32(), 0);
                return Err(-1);
            }
            let new_index = z.archive.len() + pending.adds.len() as u64;
            pending.adds.push(PendingAdd {
                name: dirname,
                data: Vec::new(),
            });
            Ok(new_index as i64)
        },
        -1,
    )
}

/// Mark the entry at `index` for deletion (applied on [`zip_close`]).
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_delete(zh: H, index: u64) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if index >= z.archive.len() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !pending.deletes.contains(&index) {
                pending.deletes.push(index);
            }
            Ok(0)
        },
        -1,
    )
}

/// Rename the entry at `index` to `name` (applied on [`zip_close`]).
///
/// # Safety
///
/// `zh` must be a valid, open handle; `name` a NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn zip_rename(zh: H, index: u64, name: *const c_char) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if index >= z.archive.len() || name.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            };
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.renames.push((index, name));
            Ok(0)
        },
        -1,
    )
}

/// Replace the entry at `index` with the data from `source` (applied on
/// [`zip_close`]).
///
/// # Safety
///
/// `zh` must be a valid, open handle; `source` a valid source from
/// [`zip_source_buffer`].
#[no_mangle]
pub unsafe extern "C" fn zip_file_replace(zh: H, index: u64, source: H, _flags: u32) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if index >= z.archive.len() || source.is_null() {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let src = source.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let data = src.data.clone();
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.replaces.push((index, data));
            Ok(0)
        },
        -1,
    )
}

/// Discard all pending changes and free the archive handle without writing.
///
/// # Safety
///
/// `zh` must be a valid, open handle not already closed.
#[no_mangle]
pub unsafe extern "C" fn zip_discard(zh: H) {
    if !zh.is_null() {
        drop(Box::from_raw(zh.cast::<Zip>()));
    }
}

// ---------------------------------------------------------------------------
// Structured error object API (zip_error_t)
// ---------------------------------------------------------------------------

/// Return a pointer to the archive's `zip_error_t`, valid until the next error
/// is set on the handle.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_get_error(zh: H) -> *mut zip_error {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let g = z.error.lock().unwrap_or_else(|e| e.into_inner());
            Ok(&g.ze as *const zip_error as *mut zip_error)
        },
        std::ptr::null_mut(),
    )
}

/// Clear the archive's error (set to `ZIP_ER_OK`).
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_error_clear(zh: H) {
    if !zh.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            z.set_err(0, 0);
        }
    }
}

/// Initialize a caller-owned `zip_error_t` to `ZIP_ER_OK`.
///
/// # Safety
///
/// `ze` must point to writable `zip_error` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_error_init(ze: *mut zip_error) {
    if !ze.is_null() {
        unsafe {
            (*ze).zip_err = 0;
            (*ze).sys_err = 0;
            (*ze).str = std::ptr::null_mut();
        }
    }
}

/// Initialize a caller-owned `zip_error_t` with a zip error code.
///
/// # Safety
///
/// `ze` must point to writable `zip_error` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_error_init_with_code(ze: *mut zip_error, code: c_int) {
    if !ze.is_null() {
        unsafe {
            (*ze).zip_err = code;
            (*ze).sys_err = 0;
            (*ze).str = CString::new(err_str(code)).unwrap_or_default().into_raw();
        }
    }
}

/// Set a caller-owned `zip_error_t`'s zip and system error codes.
///
/// # Safety
///
/// `ze` must point to writable `zip_error` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_error_set(ze: *mut zip_error, zip_err: c_int, sys_err: c_int) {
    if !ze.is_null() {
        unsafe {
            if !(*ze).str.is_null() {
                drop(CString::from_raw((*ze).str));
            }
            (*ze).zip_err = zip_err;
            (*ze).sys_err = sys_err;
            (*ze).str = CString::new(err_str(zip_err))
                .unwrap_or_default()
                .into_raw();
        }
    }
}

/// Return the error string for a `zip_error_t`, allocating it if needed.
///
/// # Safety
///
/// `ze` must point to a valid `zip_error`.
#[no_mangle]
pub unsafe extern "C" fn zip_error_strerror(ze: *mut zip_error) -> *const c_char {
    guarded(
        || {
            if ze.is_null() {
                return Err(-1);
            }
            unsafe {
                if (*ze).str.is_null() {
                    (*ze).str = CString::new(err_str((*ze).zip_err))
                        .unwrap_or_default()
                        .into_raw();
                }
                Ok((*ze).str as *const c_char)
            }
        },
        std::ptr::null(),
    )
}

/// Return the zip error code of a `zip_error_t`.
///
/// # Safety
///
/// `ze` must point to a valid `zip_error`.
#[no_mangle]
pub unsafe extern "C" fn zip_error_code_zip(ze: *const zip_error) -> c_int {
    guarded(
        || {
            if ze.is_null() {
                return Err(-1);
            }
            Ok(unsafe { (*ze).zip_err })
        },
        -1,
    )
}

/// Return the system error code of a `zip_error_t`.
///
/// # Safety
///
/// `ze` must point to a valid `zip_error`.
#[no_mangle]
pub unsafe extern "C" fn zip_error_code_system(ze: *const zip_error) -> c_int {
    guarded(
        || {
            if ze.is_null() {
                return Err(-1);
            }
            Ok(unsafe { (*ze).sys_err })
        },
        -1,
    )
}

/// Free the resources of a caller-owned `zip_error_t` and reset it.
///
/// # Safety
///
/// `ze` must point to a valid `zip_error`.
#[no_mangle]
pub unsafe extern "C" fn zip_error_fini(ze: *mut zip_error) {
    if !ze.is_null() {
        unsafe {
            if !(*ze).str.is_null() {
                drop(CString::from_raw((*ze).str));
                (*ze).str = std::ptr::null_mut();
            }
            (*ze).zip_err = 0;
            (*ze).sys_err = 0;
        }
    }
}

/// Format a `(zip_err, sys_err)` pair into `buf` (deprecated libzip helper).
/// Returns the number of bytes written (excluding the NUL).
///
/// # Safety
///
/// `buf` must point to `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_error_to_str(
    buf: *mut c_char,
    len: u64,
    ze: c_int,
    se: c_int,
) -> c_int {
    guarded(
        || {
            if buf.is_null() {
                return Err(-1);
            }
            let zs = err_str(ze);
            let s = if se != 0 {
                format!("{zs}: {se}")
            } else {
                zs.to_string()
            };
            let bytes = s.as_bytes();
            let n = (bytes.len() as u64).min(len.saturating_sub(1)) as usize;
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
                *buf.add(n) = 0;
            }
            Ok(n as c_int)
        },
        -1,
    )
}

/// Return the system-error type of a `zip_error_t` (`ZIP_ET_NONE`/`ZIP_ET_SYS`).
///
/// # Safety
///
/// `ze` must point to a valid `zip_error`.
#[no_mangle]
pub unsafe extern "C" fn zip_error_system_type(ze: *const zip_error) -> c_int {
    guarded(
        || {
            if ze.is_null() {
                return Err(-1);
            }
            Ok(if unsafe { (*ze).sys_err } != 0 {
                ZIP_ET_SYS
            } else {
                ZIP_ET_NONE
            })
        },
        -1,
    )
}

/// Deprecated: copy the archive's zip/system error codes into `zep`/`sep`.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `zep`/`sep` (if non-null) writable ints.
#[no_mangle]
pub unsafe extern "C" fn zip_error_get(zh: H, zep: *mut c_int, sep: *mut c_int) {
    if !zh.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            let g = z.error.lock().unwrap_or_else(|e| e.into_inner());
            if !zep.is_null() {
                unsafe { *zep = g.ze.zip_err };
            }
            if !sep.is_null() {
                unsafe { *sep = g.ze.sys_err };
            }
        }
    }
}

/// Set a `zip_error_t` from a source's error. Our buffer sources carry no
/// error, so this resets the error to `ZIP_ER_OK`.
///
/// # Safety
///
/// `ze` must point to writable `zip_error` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_error_set_from_source(ze: *mut zip_error, _src: H) {
    if !ze.is_null() {
        unsafe { zip_error_set(ze, 0, 0) };
    }
}

// ---------------------------------------------------------------------------
// fseek / ftell / seekability
// ---------------------------------------------------------------------------

/// Seek within an open entry. `whence` is `SEEK_SET`/`SEEK_CUR`/`SEEK_END`.
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`].
#[no_mangle]
pub unsafe extern "C" fn zip_fseek(fh: H, offset: i64, whence: c_int) -> i8 {
    guarded(
        || {
            let f = fh.cast::<ZipFile>().as_ref().ok_or(-1)?;
            let mut reader = f.reader.lock().unwrap_or_else(|e| e.into_inner());
            let size = reader.expected_size();
            let cur = reader.position();
            let target: i64 = match whence {
                SEEK_SET => offset,
                SEEK_CUR => cur as i64 + offset,
                SEEK_END => size as i64 + offset,
                _ => {
                    f.set_err(ZipErrorCode::Inval.as_i32());
                    return Err(-1);
                }
            };
            if target < 0 {
                f.set_err(ZipErrorCode::Inval.as_i32());
                return Err(-1);
            }
            reader.seek(target as u64).map_err(|_| {
                f.set_err(ZipErrorCode::Seek.as_i32());
                -1
            })?;
            Ok(0i8)
        },
        -1i8,
    )
}

/// Return the current read position within an open entry, or -1 on error.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`].
#[no_mangle]
pub unsafe extern "C" fn zip_ftell(fh: H) -> i64 {
    guarded(
        || {
            let f = fh.cast::<ZipFile>().as_ref().ok_or(-1)?;
            let reader = f.reader.lock().unwrap_or_else(|e| e.into_inner());
            Ok(reader.position() as i64)
        },
        -1,
    )
}

/// Return 1 if the open entry is seekable, 0 otherwise.
///
/// # Safety
///
/// `fh` must be a valid, open handle from [`zip_fopen`].
#[no_mangle]
pub unsafe extern "C" fn zip_file_is_seekable(fh: H) -> c_int {
    guarded(
        || {
            let f = fh.cast::<ZipFile>().as_ref().ok_or(-1)?;
            let reader = f.reader.lock().unwrap_or_else(|e| e.into_inner());
            Ok(if reader.is_seekable() { 1 } else { 0 })
        },
        -1,
    )
}

// ---------------------------------------------------------------------------
// Method-support queries
// ---------------------------------------------------------------------------

/// Return 1 if the compression `method` is supported for compression
/// (`compress != 0`) or decompression (`compress == 0`).
#[no_mangle]
pub extern "C" fn zip_compression_method_supported(method: i32, compress: c_int) -> c_int {
    match method {
        ZIP_CM_DEFAULT | ZIP_CM_STORE | ZIP_CM_DEFLATE => 1,
        ZIP_CM_BZIP2 => {
            // We can decompress bzip2 but not compress it.
            if compress != 0 {
                0
            } else {
                1
            }
        }
        _ => 0,
    }
}

/// Return 1 if the encryption `method` is supported for encoding
/// (`encode != 0`) or decoding (`encode == 0`). Supported: `ZIP_EM_NONE`,
/// `ZIP_EM_TRAD_PKWARE` (ZipCrypto), and `ZIP_EM_AES_128/192/256` (WinZip AES).
#[no_mangle]
pub extern "C" fn zip_encryption_method_supported(method: u16, _encode: c_int) -> c_int {
    if method == ZIP_EM_NONE
        || method == ZIP_EM_TRAD_PKWARE
        || method == ZIP_EM_AES_128
        || method == ZIP_EM_AES_192
        || method == ZIP_EM_AES_256
    {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Comments & extra fields (read side)
// ---------------------------------------------------------------------------

/// Return the archive (EOCD) comment, or NULL. If `lenp` is non-null, the
/// comment length is written to it.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `lenp` (if non-null) writable storage.
#[no_mangle]
pub unsafe extern "C" fn zip_get_archive_comment(
    zh: H,
    lenp: *mut c_int,
    _flags: u32,
) -> *const c_char {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let p = z
                .archive_comment
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(std::ptr::null());
            if !lenp.is_null() {
                unsafe {
                    *lenp = z
                        .archive_comment
                        .as_ref()
                        .map(|c| c.as_bytes().len() as c_int)
                        .unwrap_or(0);
                }
            }
            Ok(p)
        },
        std::ptr::null(),
    )
}

/// Return the comment of the entry at `index`, or NULL. If `lenp` is non-null,
/// the comment length is written to it.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `lenp` (if non-null) writable storage.
#[no_mangle]
pub unsafe extern "C" fn zip_file_get_comment(
    zh: H,
    index: u64,
    lenp: *mut c_int,
    _flags: u32,
) -> *const c_char {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let p = z
                .comments
                .get(index as usize)
                .and_then(|c| c.as_ref())
                .map(|c| c.as_ptr())
                .unwrap_or(std::ptr::null());
            if !lenp.is_null() {
                unsafe {
                    *lenp = z
                        .comments
                        .get(index as usize)
                        .and_then(|c| c.as_ref())
                        .map(|c| c.as_bytes().len() as c_int)
                        .unwrap_or(0);
                }
            }
            Ok(p)
        },
        std::ptr::null(),
    )
}

/// Number of extra fields of the entry at `index`, or -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_fields_count(zh: H, index: u64, _flags: u32) -> i16 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let d = z.archive.dirent(index).ok_or(-1)?;
            Ok(d.extra_fields.len() as i16)
        },
        -1,
    )
}

/// Number of extra fields with id `id` of the entry at `index`, or -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_fields_count_by_id(
    zh: H,
    index: u64,
    id: u16,
    _flags: u32,
) -> i16 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let d = z.archive.dirent(index).ok_or(-1)?;
            Ok(d.extra_fields.iter().filter(|(i, _)| *i == id).count() as i16)
        },
        -1,
    )
}

/// Return a pointer to the `idx`-th extra field of the entry at `index` (or
/// NULL). If `idxp` is non-null, the index of the returned field is written to
/// it; if `lenp` is non-null, the field's length is written to it.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `idxp`/`lenp` (if non-null) writable.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_field_get(
    zh: H,
    index: u64,
    id: u16,
    idxp: *mut u16,
    lenp: *mut u16,
    _flags: u32,
) -> *const u8 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let d = z.archive.dirent(index).ok_or(-1)?;
            let mut n = 0u16;
            let want = if idxp.is_null() { 0 } else { unsafe { *idxp } };
            let mut found: Option<&Vec<u8>> = None;
            for (i, data) in &d.extra_fields {
                if *i == id {
                    if n == want {
                        found = Some(data);
                        break;
                    }
                    n += 1;
                }
            }
            match found {
                Some(data) => {
                    if !idxp.is_null() {
                        unsafe { *idxp = n };
                    }
                    if !lenp.is_null() {
                        unsafe { *lenp = data.len() as u16 };
                    }
                    Ok(data.as_ptr())
                }
                None => Ok(std::ptr::null()),
            }
        },
        std::ptr::null(),
    )
}

/// Return a pointer to the `idx`-th extra field with id `id` of the entry at
/// `index` (or NULL). If `lenp` is non-null, the field's length is written to
/// it.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `lenp` (if non-null) writable.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_field_get_by_id(
    zh: H,
    index: u64,
    id: u16,
    idx: u16,
    lenp: *mut u16,
    _flags: u32,
) -> *const u8 {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            let d = z.archive.dirent(index).ok_or(-1)?;
            let mut n = 0u16;
            let mut found: Option<&Vec<u8>> = None;
            for (i, data) in &d.extra_fields {
                if *i == id {
                    if n == idx {
                        found = Some(data);
                        break;
                    }
                    n += 1;
                }
            }
            match found {
                Some(data) => {
                    if !lenp.is_null() {
                        unsafe { *lenp = data.len() as u16 };
                    }
                    Ok(data.as_ptr())
                }
                None => Ok(std::ptr::null()),
            }
        },
        std::ptr::null(),
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
    use std::path::PathBuf;
    use zip_core::constant::magic;
    use zip_core::{write_archive, ArchiveFile, CompressOptions};

    /// `err_str` must return the EXACT libzip message string (sentence-case,
    /// wording, capitalization) for every error code, matching libzip's
    /// `_zip_err_str[]` in `zip_err_str.c`. This is the parity the migration
    /// audit flagged as missing (NOZIP/TRUNCATED_ZIP/EF_TOO_LARGE/OPEN/INCONS
    /// were wrong or fell through to a generic `"zip error"`).
    #[test]
    fn err_str_matches_libzip_exactly() {
        // code -> libzip `_zip_err_str[]` string (indexed by error code).
        let expected: &[(i32, &str)] = &[
            (0, "No error"),
            (1, "Multi-disk zip archives not supported"),
            (2, "Renaming temporary file failed"),
            (3, "Closing zip archive failed"),
            (4, "Seek error"),
            (5, "Read error"),
            (6, "Write error"),
            (7, "CRC error"),
            (8, "Containing zip archive was closed"),
            (9, "No such file"),
            (10, "File already exists"),
            (11, "Can't open file"),
            (12, "Failure to create temporary file"),
            (13, "Zlib error"),
            (14, "Malloc failure"),
            (15, "Entry has been changed"),
            (16, "Compression method not supported"),
            (17, "Premature end of file"),
            (18, "Invalid argument"),
            (19, "Not a zip archive"),
            (20, "Internal error"),
            (21, "Zip archive inconsistent"),
            (22, "Can't remove file"),
            (23, "Entry has been deleted"),
            (24, "Encryption method not supported"),
            (25, "Read-only archive"),
            (26, "No password provided"),
            (27, "Wrong password provided"),
            (28, "Operation not supported"),
            (29, "Resource still in use"),
            (30, "Tell error"),
            (31, "Compressed data invalid"),
            (32, "Operation cancelled"),
            (33, "Unexpected length of data"),
            (34, "Not allowed in torrentzip"),
            (35, "Possibly truncated or corrupted zip archive"),
            (36, "Extra fields too large"),
            // Rust-port zip-bomb guards (not in libzip's table).
            (37, "Decompressed data exceeds the size limit"),
            (38, "Central directory too large"),
        ];
        for &(code, want) in expected {
            assert_eq!(
                err_str(code),
                want,
                "err_str({code}) must match libzip exactly"
            );
        }
    }

    /// `zip_open` must reject a file larger than `MAX_OPEN_FILE_SIZE` with a
    /// clear error instead of performing an unbounded whole-file read.
    #[test]
    fn zip_open_rejects_oversized_file() {
        let path = temp_path("oversize");
        // Write a sparse file larger than the cap (no real disk usage).
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_OPEN_FILE_SIZE + 1).unwrap();
        drop(f);

        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut errp: c_int = 0;
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, &mut errp) };
        assert!(zh.is_null(), "oversized file must be rejected");
        assert_eq!(errp, ZipErrorCode::Memory.as_i32());

        std::fs::remove_file(&path).ok();
    }

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
            flags: 0,
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
            flags: 0,
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

        // Missing entry -> NULL handle and a "No such file" strerror.
        let missing = CString::new("nope.txt").unwrap();
        assert!(unsafe { zip_fopen(zh, missing.as_ptr(), 0) }.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "No such file");

        // NULL name -> NULL handle, invalid-argument strerror.
        assert!(unsafe { zip_fopen(zh, std::ptr::null(), 0) }.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "Invalid argument");

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
        assert_eq!(sb.valid, 0xFF); // the 8 real ZIP_STAT_* bits
        assert_eq!(sb.encryption_method, 0); // ZIP_EM_NONE
        assert_eq!(sb.flags, 0);

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
            flags: 5,
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
        assert_eq!(z.flags, 0);

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

    /// `zip_name_locate` returns the index of the first entry with the given
    /// name, or -1 (with the archive error set) when not found.
    #[test]
    fn ffi_name_locate() {
        let path = temp_path("namelocate");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        assert_eq!(
            unsafe { zip_name_locate(zh, CString::new("b.bin").unwrap().as_ptr(), 0) },
            1
        );
        assert_eq!(
            unsafe { zip_name_locate(zh, CString::new("a.txt").unwrap().as_ptr(), 0) },
            0
        );
        // Missing -> -1 and the archive error is set to "No such file".
        assert_eq!(
            unsafe { zip_name_locate(zh, CString::new("nope").unwrap().as_ptr(), 0) },
            -1
        );
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "No such file");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// A C-ABI write -> read round-trip: create a new archive with
    /// `ZIP_CREATE`, add a file and a directory, close (which writes), reopen
    /// and read the added content back.
    #[test]
    fn ffi_write_roundtrip() {
        let path = temp_path("write");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), ZIP_CREATE, std::ptr::null_mut()) };
        assert!(!zh.is_null(), "zip_open ZIP_CREATE failed");

        let data = b"hello from the C ABI write path".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        let idx = unsafe { zip_file_add(zh, CString::new("hello.txt").unwrap().as_ptr(), src, 0) };
        assert!(idx >= 0, "zip_file_add failed");
        unsafe { zip_source_free(src) };

        let didx = unsafe { zip_dir_add(zh, CString::new("subdir").unwrap().as_ptr(), 0) };
        assert!(didx >= 0, "zip_dir_add failed");

        assert_eq!(unsafe { zip_close(zh) }, 0);

        // Reopen and verify.
        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        assert_eq!(unsafe { zip_get_num_entries(zh2, 0) }, 2);
        assert_eq!(cstr(unsafe { zip_get_name(zh2, 0, 0) }), "hello.txt");
        assert_eq!(cstr(unsafe { zip_get_name(zh2, 1, 0) }), "subdir/");

        let fh = unsafe { zip_fopen_index(zh2, 0, 0) };
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
        assert_eq!(out, data);
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_file_add` with a duplicate name (no `ZIP_FL_OVERWRITE`) sets
    /// `ZIP_ER_EXISTS`; with `ZIP_FL_OVERWRITE` it replaces the existing entry.
    #[test]
    fn ffi_file_add_overwrite_and_exists() {
        let path = temp_path("add_overwrite");
        build_zip(&path); // a.txt, b.bin, empty.txt
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());

        // Duplicate name without overwrite -> -1 + Exists.
        let data = b"dup".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        assert_eq!(
            unsafe { zip_file_add(zh, CString::new("a.txt").unwrap().as_ptr(), src, 0) },
            -1
        );
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "File already exists");
        unsafe { zip_source_free(src) };

        // With ZIP_FL_OVERWRITE -> replaces index 0.
        let newdata = b"overwritten".to_vec();
        let src2 = unsafe {
            zip_source_buffer(
                zh,
                newdata.as_ptr() as *const c_void,
                newdata.len() as u64,
                0,
            )
        };
        assert!(!src2.is_null());
        assert_eq!(
            unsafe {
                zip_file_add(
                    zh,
                    CString::new("a.txt").unwrap().as_ptr(),
                    src2,
                    ZIP_FL_OVERWRITE,
                )
            },
            0
        );
        unsafe { zip_source_free(src2) };
        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        assert_eq!(unsafe { zip_get_num_entries(zh2, 0) }, 3);
        let fh = unsafe { zip_fopen_index(zh2, 0, 0) };
        assert!(!fh.is_null());
        let mut out = Vec::new();
        let mut buf = [0u8; 32];
        loop {
            let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 32) };
            assert!(n >= 0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        assert_eq!(out, newdata);
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_delete`/`zip_rename`/`zip_file_replace` are applied on close.
    #[test]
    fn ffi_edit_ops() {
        let path = temp_path("edit");
        build_zip(&path); // a.txt, b.bin, empty.txt
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());

        assert_eq!(unsafe { zip_delete(zh, 2) }, 0); // delete empty.txt
        assert_eq!(
            unsafe { zip_rename(zh, 0, CString::new("renamed.txt").unwrap().as_ptr()) },
            0
        );
        let newdata = b"replacement content".to_vec();
        let src = unsafe {
            zip_source_buffer(
                zh,
                newdata.as_ptr() as *const c_void,
                newdata.len() as u64,
                0,
            )
        };
        assert!(!src.is_null());
        assert_eq!(unsafe { zip_file_replace(zh, 1, src, 0) }, 0);
        unsafe { zip_source_free(src) };

        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        assert_eq!(unsafe { zip_get_num_entries(zh2, 0) }, 2);
        assert_eq!(cstr(unsafe { zip_get_name(zh2, 0, 0) }), "renamed.txt");
        assert_eq!(cstr(unsafe { zip_get_name(zh2, 1, 0) }), "b.bin");

        let fh = unsafe { zip_fopen_index(zh2, 1, 0) };
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
        assert_eq!(out, newdata);
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_discard` frees the handle without writing anything to disk.
    #[test]
    fn ffi_discard_does_not_write() {
        let path = temp_path("discard");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), ZIP_CREATE, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let data = b"discarded".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        assert!(unsafe { zip_file_add(zh, CString::new("x.txt").unwrap().as_ptr(), src, 0) } >= 0);
        unsafe { zip_source_free(src) };
        unsafe { zip_discard(zh) };
        assert!(!path.exists(), "zip_discard must not write the archive");
    }

    /// The structured `zip_error_t` object API: init/set/strerror/code/fini on
    /// a caller-owned object, plus `zip_get_error`/`zip_error_clear` on the
    /// archive's error object.
    #[test]
    fn ffi_error_object_api() {
        let mut ze: zip_error = zip_error {
            zip_err: 0,
            sys_err: 0,
            str: std::ptr::null_mut(),
        };
        unsafe { zip_error_init(&mut ze) };
        assert_eq!(unsafe { zip_error_code_zip(&ze) }, 0);
        unsafe { zip_error_set(&mut ze, ZipErrorCode::Noent.as_i32(), 0) };
        assert_eq!(
            unsafe { zip_error_code_zip(&ze) },
            ZipErrorCode::Noent.as_i32()
        );
        assert_eq!(cstr(unsafe { zip_error_strerror(&mut ze) }), "No such file");
        assert_eq!(unsafe { zip_error_system_type(&ze) }, ZIP_ET_NONE);
        unsafe { zip_error_fini(&mut ze) };
        assert!(ze.str.is_null());

        let mut ze2: zip_error = zip_error {
            zip_err: 0,
            sys_err: 0,
            str: std::ptr::null_mut(),
        };
        unsafe { zip_error_init_with_code(&mut ze2, ZipErrorCode::Inval.as_i32()) };
        assert_eq!(
            unsafe { zip_error_code_zip(&ze2) },
            ZipErrorCode::Inval.as_i32()
        );
        assert_eq!(
            cstr(unsafe { zip_error_strerror(&mut ze2) }),
            "Invalid argument"
        );
        unsafe { zip_error_fini(&mut ze2) };

        // Archive error object via zip_get_error.
        let path = temp_path("err_obj");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        assert!(unsafe { zip_fopen(zh, CString::new("nope").unwrap().as_ptr(), 0) }.is_null());
        let ze3 = unsafe { zip_get_error(zh) };
        assert!(!ze3.is_null());
        assert_eq!(
            unsafe { zip_error_code_zip(ze3) },
            ZipErrorCode::Noent.as_i32()
        );
        assert_eq!(cstr(unsafe { zip_error_strerror(ze3) }), "No such file");
        unsafe { zip_error_clear(zh) };
        assert_eq!(unsafe { zip_error_code_zip(ze3) }, 0);
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_fseek`/`zip_ftell`/`zip_file_is_seekable` on an open entry.
    #[test]
    fn ffi_fseek_ftell_seekable() {
        let path = temp_path("seek");
        let content = b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec();
        let files = vec![zip_core::ArchiveFile::new("s.txt", content.clone())];
        let bytes = zip_core::write_archive(&files, &zip_core::CompressOptions::default()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(!fh.is_null());
        assert_eq!(unsafe { zip_file_is_seekable(fh) }, 1);
        assert_eq!(unsafe { zip_ftell(fh) }, 0);

        let mut buf = [0u8; 5];
        assert_eq!(
            unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 5) },
            5
        );
        assert_eq!(&buf, b"01234");
        assert_eq!(unsafe { zip_ftell(fh) }, 5);

        assert_eq!(unsafe { zip_fseek(fh, 10, SEEK_SET) }, 0);
        assert_eq!(unsafe { zip_ftell(fh) }, 10);
        assert_eq!(
            unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 5) },
            5
        );
        assert_eq!(&buf, b"abcde");

        assert_eq!(unsafe { zip_fseek(fh, 5, SEEK_CUR) }, 0);
        assert_eq!(unsafe { zip_ftell(fh) }, 20);
        assert_eq!(
            unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 5) },
            5
        );
        assert_eq!(&buf, b"klmno");

        assert_eq!(unsafe { zip_fseek(fh, -3, SEEK_END) }, 0);
        assert_eq!(unsafe { zip_ftell(fh) }, (content.len() - 3) as i64);
        assert_eq!(
            unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 3) },
            3
        );
        assert_eq!(&buf[..3], b"xyz");

        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// `zip_compression_method_supported` / `zip_encryption_method_supported`.
    #[test]
    fn encryption_method_supported() {
        assert_eq!(zip_compression_method_supported(ZIP_CM_STORE, 1), 1);
        assert_eq!(zip_compression_method_supported(ZIP_CM_DEFLATE, 1), 1);
        assert_eq!(zip_compression_method_supported(ZIP_CM_DEFAULT, 1), 1);
        assert_eq!(zip_compression_method_supported(ZIP_CM_BZIP2, 1), 0);
        assert_eq!(zip_compression_method_supported(ZIP_CM_BZIP2, 0), 1);
        assert_eq!(zip_compression_method_supported(99, 1), 0);
        assert_eq!(zip_encryption_method_supported(ZIP_EM_NONE, 1), 1);
        assert_eq!(zip_encryption_method_supported(ZIP_EM_TRAD_PKWARE, 1), 1);
        // Phase 2: WinZip AES methods are supported.
        assert_eq!(zip_encryption_method_supported(ZIP_EM_AES_128, 1), 1);
        assert_eq!(zip_encryption_method_supported(ZIP_EM_AES_192, 1), 1);
        assert_eq!(zip_encryption_method_supported(ZIP_EM_AES_256, 1), 1);
        assert_eq!(zip_encryption_method_supported(99, 1), 0);
    }

    /// Build a small zip with one stored entry carrying an extra field, a file
    /// comment, and an archive comment.
    fn build_zip_with_meta(path: &std::path::Path) {
        let name = b"a.txt";
        let content = b"hello";
        let crc = crc32fast::hash(content);
        let extra: &[u8] = &[0xFE, 0xCA, 3, 0, 1, 2, 3]; // id=0xCAFE, len=3, data=[1,2,3]
        let file_comment = b"file comment";
        let archive_comment = b"archive comment";
        let mut v = Vec::new();
        v.extend_from_slice(&magic::LOCAL);
        v.extend_from_slice(&20u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&crc.to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(name);
        v.extend_from_slice(extra);
        v.extend_from_slice(content);
        let cdir_offset = v.len() as u64;
        v.extend_from_slice(&magic::CENTRAL);
        v.extend_from_slice(&20u16.to_le_bytes());
        v.extend_from_slice(&20u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&crc.to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(content.len() as u32).to_le_bytes());
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(&(file_comment.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(name);
        v.extend_from_slice(extra);
        v.extend_from_slice(file_comment);
        let cdir_size = (v.len() - cdir_offset as usize) as u64;
        v.extend_from_slice(&magic::EOCD);
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(cdir_size as u32).to_le_bytes());
        v.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
        v.extend_from_slice(&(archive_comment.len() as u16).to_le_bytes());
        v.extend_from_slice(archive_comment);
        std::fs::write(path, v).unwrap();
    }

    /// Archive/file comments and extra fields are readable through the ABI.
    #[test]
    fn ffi_comments_and_extra_fields_read() {
        let path = temp_path("meta");
        build_zip_with_meta(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());

        let mut len: c_int = 0;
        let ac = unsafe { zip_get_archive_comment(zh, &mut len, 0) };
        assert_eq!(cstr(ac), "archive comment");
        assert_eq!(len, "archive comment".len() as c_int);

        let mut flen: c_int = 0;
        let fc = unsafe { zip_file_get_comment(zh, 0, &mut flen, 0) };
        assert_eq!(cstr(fc), "file comment");
        assert_eq!(flen, "file comment".len() as c_int);

        assert_eq!(unsafe { zip_file_extra_fields_count(zh, 0, 0) }, 1);
        assert_eq!(
            unsafe { zip_file_extra_fields_count_by_id(zh, 0, 0xCAFE, 0) },
            1
        );
        let mut elen: u16 = 0;
        let ep = unsafe { zip_file_extra_field_get_by_id(zh, 0, 0xCAFE, 0, &mut elen, 0) };
        assert!(!ep.is_null());
        assert_eq!(elen, 3);
        let data = unsafe { std::slice::from_raw_parts(ep, elen as usize) };
        assert_eq!(data, &[1, 2, 3]);

        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    // ---- Phase 1: ZipCrypto (traditional PKWARE) encryption ABI ----

    /// Write an encrypted archive to `path` (all entries ZipCrypto-encrypted)
    /// using `password`, via the Rust core writer.
    fn build_encrypted_zip(path: &std::path::Path, password: &[u8]) {
        let files = vec![
            zip_core::ArchiveFile::new("secret.txt", b"top secret ffi content ".repeat(20)),
            zip_core::ArchiveFile::new("payload.bin", vec![0xAB; 512]),
        ];
        let encrypt = vec![true; files.len()];
        let bytes =
            zip_core::write_archive_encrypted(&files, &zip_core::CompressOptions::default(), password, &encrypt)
                .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Read an open entry fully via the extern ABI.
    unsafe fn read_ffi_full(fh: H) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 64) };
            assert!(n >= 0, "zip_fread error");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        out
    }

    /// TC-3: wrong password -> ZIP_ER_WRONGPASS (27) + exact C string.
    #[test]
    fn wrong_password() {
        let path = temp_path("wrongpw");
        build_encrypted_zip(&path, b"kzip-test-password");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let pw = CString::new("wrong-password").unwrap();
        let fh = unsafe { zip_fopen_index_encrypted(zh, 0, 0, pw.as_ptr()) };
        assert!(fh.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "Wrong password provided");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4: no password -> ZIP_ER_NOPASS (26) + exact C string.
    #[test]
    fn no_password() {
        let path = temp_path("nopass");
        build_encrypted_zip(&path, b"kzip-test-password");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        // fopen with no password set: encrypted entry -> NOPASS.
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(fh.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "No password provided");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-6: zip_file_set_encryption + zip_set_default_password round-trip.
    #[test]
    fn encryption_round_trip() {
        let path = temp_path("enc_roundtrip");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        // Create a new archive, set default password, mark entry encrypted.
        let zh = unsafe { zip_open(cpath.as_ptr(), ZIP_CREATE, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        assert_eq!(
            unsafe {
                zip_set_default_password(zh, CString::new("kzip-test-password").unwrap().as_ptr())
            },
            0
        );
        let data = b"round trip encrypted payload".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        let idx = unsafe { zip_file_add(zh, CString::new("enc.txt").unwrap().as_ptr(), src, 0) };
        assert!(idx >= 0);
        unsafe { zip_source_free(src) };

        // Set encryption to traditional PKWARE.
        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, ZIP_EM_TRAD_PKWARE) }, 0);
        // Unsupported method -> ZIP_ER_ENCRNOTSUPP (24).
        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, 99) }, -1);
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "Encryption method not supported");
        // Restore supported method.
        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, ZIP_EM_TRAD_PKWARE) }, 0);

        assert_eq!(unsafe { zip_close(zh) }, 0);

        // Reopen: stat reports ZIP_EM_TRAD_PKWARE (1).
        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, idx as u64, 0, &mut sb) }, 0);
        assert_eq!(sb.encryption_method, ZIP_EM_TRAD_PKWARE);

        // fopen with no password -> NOPASS.
        assert!(unsafe { zip_fopen_index(zh2, idx as u64, 0) }.is_null());

        // fopen_index_encrypted with correct password -> original bytes.
        let pw = CString::new("kzip-test-password").unwrap();
        let fh = unsafe { zip_fopen_index_encrypted(zh2, idx as u64, 0, pw.as_ptr()) };
        assert!(!fh.is_null());
        assert_eq!(unsafe { read_ffi_full(fh) }, data);
        unsafe { zip_fclose(fh) };

        // Set default password on the reopened archive and fopen normally.
        assert_eq!(
            unsafe { zip_set_default_password(zh2, pw.as_ptr()) },
            0
        );
        let fh2 = unsafe { zip_fopen_index(zh2, idx as u64, 0) };
        assert!(!fh2.is_null(), "default password should decrypt on fopen");
        assert_eq!(unsafe { read_ffi_full(fh2) }, data);
        unsafe { zip_fclose(fh2) };

        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-7: zip_stat reports ZIP_EM_TRAD_PKWARE for encrypted entries;
    /// unencrypted still reports ZIP_EM_NONE.
    #[test]
    fn stat_encryption_method_trad_pkw() {
        // Encrypted archive.
        let path = temp_path("stat_enc");
        build_encrypted_zip(&path, b"kzip-test-password");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh, 0, 0, &mut sb) }, 0);
        assert_eq!(sb.encryption_method, ZIP_EM_TRAD_PKWARE);
        assert_eq!(sb.valid, 0xFF);
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();

        // Unencrypted archive still reports 0.
        let path2 = temp_path("stat_plain");
        build_zip(&path2);
        let cpath2 = CString::new(path2.to_string_lossy().as_bytes()).unwrap();
        let zh2 = unsafe { zip_open(cpath2.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb2 = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, 0, 0, &mut sb2) }, 0);
        assert_eq!(sb2.encryption_method, ZIP_EM_NONE);
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path2).ok();
    }

    /// All four Phase 1 symbols must resolve via libloading/dlopen.
    #[test]
    fn abi_symbols_present() {
        // Locate the cdylib in the workspace target dir (the crate's own
        // CWD is the package dir, so use an absolute path from the manifest).
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            manifest.join("../../target/release/zip.dll"),
            manifest.join("../../target/debug/zip.dll"),
        ];
        let path = candidates
            .iter()
            .find(|p| p.exists())
            .expect("zip.dll not found for ABI probe");
        let lib = unsafe { libloading::Library::new(path) }.expect("load zip.dll for ABI probe");
        for sym in [
            "zip_fopen_encrypted",
            "zip_fopen_index_encrypted",
            "zip_file_set_encryption",
            "zip_set_default_password",
            "zip_encryption_method_supported",
        ] {
            let found: Result<libloading::Symbol<*mut libc::c_void>, _> =
                unsafe { lib.get(sym.as_bytes()) };
            assert!(found.is_ok(), "symbol {sym} missing from cdylib");
        }
    }

    /// `zip_fopen_encrypted` by name with the correct password reads the entry.
    #[test]
    fn fopen_encrypted_by_name() {
        let path = temp_path("fopen_enc_name");
        build_encrypted_zip(&path, b"kzip-test-password");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let name = CString::new("secret.txt").unwrap();
        let pw = CString::new("kzip-test-password").unwrap();
        let fh = unsafe { zip_fopen_encrypted(zh, name.as_ptr(), 0, pw.as_ptr()) };
        assert!(!fh.is_null(), "zip_fopen_encrypted failed");
        let out = unsafe { read_ffi_full(fh) };
        assert_eq!(out, b"top secret ffi content ".repeat(20));
        unsafe { zip_fclose(fh) };
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    // ---- Phase 2: WinZip AES ABI ----

    /// Write a WinZip AES-encrypted archive (all entries encrypted with
    /// `method`) via the Rust core writer, to `path`.
    fn build_aes_zip(path: &std::path::Path, password: &[u8], method: u16) {
        let files = vec![
            zip_core::ArchiveFile::new("aes.txt", b"winzip aes ffi payload ".repeat(24)),
            zip_core::ArchiveFile::new("aes.bin", vec![0x7F; 384]),
        ];
        let methods = vec![method; files.len()];
        let bytes = zip_core::write_archive_encrypted_methods(
            &files,
            &zip_core::CompressOptions::default(),
            password,
            &methods,
        )
        .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// TC-3: wrong password -> ZIP_ER_WRONGPASS (27) + exact C string.
    #[test]
    fn aes_wrong_password() {
        let path = temp_path("aes_wrongpw");
        build_aes_zip(&path, b"kzip-test-password", ZIP_EM_AES_256);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let pw = CString::new("wrong-password").unwrap();
        let fh = unsafe { zip_fopen_index_encrypted(zh, 0, 0, pw.as_ptr()) };
        assert!(fh.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "Wrong password provided");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4: no password -> ZIP_ER_NOPASS (26) + exact C string.
    #[test]
    fn aes_no_password() {
        let path = temp_path("aes_nopass");
        build_aes_zip(&path, b"kzip-test-password", ZIP_EM_AES_256);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        // No default password set -> NOPASS.
        let fh = unsafe { zip_fopen_index(zh, 0, 0) };
        assert!(fh.is_null());
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "No password provided");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-6: zip_stat reports ZIP_EM_AES_128/192/256 for AES entries; an
    /// unencrypted entry still reports 0.
    #[test]
    fn stat_encryption_method_aes() {
        for (method, expected) in [
            (ZIP_EM_AES_128, 257u16),
            (ZIP_EM_AES_192, 258u16),
            (ZIP_EM_AES_256, 259u16),
        ] {
            let path = temp_path(&format!("aes_stat_{method}"));
            build_aes_zip(&path, b"kzip-test-password", method);
            let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
            assert!(!zh.is_null());
            let mut sb = zero_stat();
            assert_eq!(unsafe { zip_stat_index(zh, 0, 0, &mut sb) }, 0);
            assert_eq!(sb.encryption_method, expected, "method {method:#06x}");
            // AES AE-2 entries have no valid CRC, so the CRC stat bit is clear.
            assert_eq!(sb.valid, 0xDF);
            unsafe { zip_close(zh) };
            std::fs::remove_file(&path).ok();
        }

        // Unencrypted entry still reports 0.
        let path2 = temp_path("aes_stat_plain");
        build_zip(&path2);
        let cpath2 = CString::new(path2.to_string_lossy().as_bytes()).unwrap();
        let zh2 = unsafe { zip_open(cpath2.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb2 = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, 0, 0, &mut sb2) }, 0);
        assert_eq!(sb2.encryption_method, ZIP_EM_NONE);
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path2).ok();
    }

    /// TC-10: full C-ABI round-trip for AES-256 — set default password, add a
    /// file, set encryption to AES-256, write via zip_close, reopen, stat
    /// reports AES-256, and zip_fopen_index_encrypted returns the original
    /// bytes.
    #[test]
    fn aes_round_trip_abi() {
        let path = temp_path("aes_roundtrip");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let zh = unsafe { zip_open(cpath.as_ptr(), ZIP_CREATE, std::ptr::null_mut()) };
        assert!(!zh.is_null(), "zip_open ZIP_CREATE failed");
        assert_eq!(
            unsafe {
                zip_set_default_password(zh, CString::new("kzip-test-password").unwrap().as_ptr())
            },
            0
        );
        let data = b"aes round trip encrypted abi payload".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        let idx = unsafe { zip_file_add(zh, CString::new("aes.txt").unwrap().as_ptr(), src, 0) };
        assert!(idx >= 0, "zip_file_add failed");
        unsafe { zip_source_free(src) };

        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, ZIP_EM_AES_256) }, 0);
        // Unsupported method -> ZIP_ER_ENCRNOTSUPP (24).
        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, 99) }, -1);
        assert_eq!(
            cstr(unsafe { zip_strerror(zh) }),
            "Encryption method not supported"
        );
        assert_eq!(unsafe { zip_file_set_encryption(zh, idx as u64, ZIP_EM_AES_256) }, 0);

        assert_eq!(unsafe { zip_close(zh) }, 0);

        // Reopen: stat reports AES-256.
        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, idx as u64, 0, &mut sb) }, 0);
        assert_eq!(sb.encryption_method, ZIP_EM_AES_256);

        // No password -> NOPASS.
        assert!(unsafe { zip_fopen_index(zh2, idx as u64, 0) }.is_null());

        // Correct password -> original bytes.
        let pw = CString::new("kzip-test-password").unwrap();
        let fh = unsafe { zip_fopen_index_encrypted(zh2, idx as u64, 0, pw.as_ptr()) };
        assert!(!fh.is_null(), "zip_fopen_index_encrypted failed");
        assert_eq!(unsafe { read_ffi_full(fh) }, data);
        unsafe { zip_fclose(fh) };

        // Set default password on reopen and fopen normally.
        assert_eq!(unsafe { zip_set_default_password(zh2, pw.as_ptr()) }, 0);
        let fh2 = unsafe { zip_fopen_index(zh2, idx as u64, 0) };
        assert!(!fh2.is_null(), "default password should decrypt on fopen");
        assert_eq!(unsafe { read_ffi_full(fh2) }, data);
        unsafe { zip_fclose(fh2) };

        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-2: corrupted AES ciphertext -> ZIP_ER_CRC (7) via the C ABI.
    #[test]
    fn aes_integrity_corruption_abi() {
        let files = vec![zip_core::ArchiveFile::new(
            "secret.txt",
            b"aes integrity check payload ".repeat(30),
        )];
        let methods = vec![ZIP_EM_AES_256; 1];
        let bytes = zip_core::write_archive_encrypted_methods(
            &files,
            &zip_core::CompressOptions::default(),
            b"kzip-test-password",
            &methods,
        )
        .unwrap();
        // Corrupt a ciphertext byte (not the header/HMAC): local header (30)
        // + name (10) + AES extra (11) + salt (16, AES-256) + verify (2) =>
        // first ciphertext byte at offset 69.
        let cipher_pos = 30 + 10 + 11 + 16 + 2;
        let mut corrupted = bytes.clone();
        corrupted[cipher_pos] ^= 0xFF;

        let path = temp_path("aes_corrupt");
        std::fs::write(&path, &corrupted).unwrap();
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        let pw = CString::new("kzip-test-password").unwrap();
        let fh = unsafe { zip_fopen_index_encrypted(zh, 0, 0, pw.as_ptr()) };
        assert!(fh.is_null(), "corrupted AES ciphertext must fail to open");
        assert_eq!(cstr(unsafe { zip_strerror(zh) }), "CRC error");
        unsafe { zip_close(zh) };
        std::fs::remove_file(&path).ok();
    }
}
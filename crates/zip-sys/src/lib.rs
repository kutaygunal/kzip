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
    write_archive, write_archive_full, Archive, ArchiveFile, CompressOptions, EntryReader, Stat,
    ZipErrorCode,
};
use zip_core::constant::CompressionMethod;

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

/// Extra-field placement flags (`ZIP_FL_LOCAL` / `ZIP_FL_CENTRAL`) are
/// intentionally not applied: this port stores user extra fields in both the
/// local header and the central directory (the common, interoperable case).

/// Host system constant (`ZIP_OPSYS_UNIX`).
const ZIP_OPSYS_UNIX: u8 = 3;

/// Extra-field sentinel indices (`ZIP_EXTRA_FIELD_ALL` / `ZIP_EXTRA_FIELD_NEW`).
const ZIP_EXTRA_FIELD_ALL: u16 = u16::MAX;
const ZIP_EXTRA_FIELD_NEW: u16 = u16::MAX;

/// Internal extra-field IDs the writer manages itself (not user data).
const ZIP_EF_ZIP64: u16 = 0x0001;
const ZIP_EF_WINZIP_AES: u16 = 0x9901;

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

/// `zip_source_cmd` values (mirror libzip `lib/zip.h` `enum zip_source_cmd`).
const ZIP_SOURCE_OPEN: c_int = 0;
const ZIP_SOURCE_READ: c_int = 1;
const ZIP_SOURCE_CLOSE: c_int = 2;
const ZIP_SOURCE_STAT: c_int = 3;
const ZIP_SOURCE_ERROR: c_int = 4;
const ZIP_SOURCE_FREE: c_int = 5;
const ZIP_SOURCE_SEEK: c_int = 6;
const ZIP_SOURCE_TELL: c_int = 7;
const ZIP_SOURCE_BEGIN_WRITE: c_int = 8;
const ZIP_SOURCE_COMMIT_WRITE: c_int = 9;
const ZIP_SOURCE_ROLLBACK_WRITE: c_int = 10;
const ZIP_SOURCE_WRITE: c_int = 11;
const ZIP_SOURCE_SEEK_WRITE: c_int = 12;
const ZIP_SOURCE_TELL_WRITE: c_int = 13;
const ZIP_SOURCE_SUPPORTS: c_int = 14;
const ZIP_SOURCE_REMOVE: c_int = 15;
const ZIP_SOURCE_RESERVED_1: c_int = 16;
const ZIP_SOURCE_BEGIN_WRITE_CLONING: c_int = 17;
const ZIP_SOURCE_ACCEPT_EMPTY: c_int = 18;
const ZIP_SOURCE_GET_FILE_ATTRIBUTES: c_int = 19;
const ZIP_SOURCE_SUPPORTS_REOPEN: c_int = 20;
const ZIP_SOURCE_GET_DOS_TIME: c_int = 21;
const ZIP_SOURCE_AT_EOF: c_int = 22;

/// `ZIP_SOURCE_MAKE_COMMAND_BITMASK(cmd)` — a single-bit command mask.
#[inline]
const fn src_bit(cmd: c_int) -> i64 {
    1i64 << cmd
}

/// `ZIP_SOURCE_SUPPORTS_READABLE` / `SEEKABLE` bitmasks.
const SRC_READABLE: i64 = src_bit(ZIP_SOURCE_OPEN)
    | src_bit(ZIP_SOURCE_READ)
    | src_bit(ZIP_SOURCE_CLOSE)
    | src_bit(ZIP_SOURCE_STAT)
    | src_bit(ZIP_SOURCE_ERROR)
    | src_bit(ZIP_SOURCE_FREE);
const SRC_SEEKABLE: i64 = SRC_READABLE
    | src_bit(ZIP_SOURCE_SEEK)
    | src_bit(ZIP_SOURCE_TELL)
    | src_bit(ZIP_SOURCE_SUPPORTS);

/// `ZIP_STAT_*` valid bits.
const ZIP_STAT_NAME: u64 = 0x0001;
const ZIP_STAT_INDEX: u64 = 0x0002;
const ZIP_STAT_SIZE: u64 = 0x0004;
const ZIP_STAT_COMP_SIZE: u64 = 0x0008;
const ZIP_STAT_MTIME: u64 = 0x0010;
const ZIP_STAT_CRC: u64 = 0x0020;
const ZIP_STAT_COMP_METHOD: u64 = 0x0040;
const ZIP_STAT_ENCRYPTION_METHOD: u64 = 0x0080;
const ZIP_STAT_FLAGS: u64 = 0x0100;

/// `zip_source_zip` / `zip_source_file` flags.
const ZIP_FL_COMPRESSED: u32 = 4;
const ZIP_FL_ENCRYPTED: u32 = 32;
const ZIP_FL_UNCHANGED: u32 = 8;

/// `ZIP_LENGTH_UNCHECKED` sentinel (`-2`).
const ZIP_LENGTH_UNCHECKED: i64 = -2;

/// Argument struct for the `ZIP_SOURCE_SEEK` command (`zip_source_args_seek`).
#[repr(C)]
struct ZipSourceArgsSeek {
    offset: i64,
    whence: c_int,
}

/// A user-supplied `zip_source_callback` (`void*ud, void*data, zip_uint64_t
/// len, zip_source_cmd_t cmd) -> zip_int64_t`).
type ZipSourceCallback =
    Option<unsafe extern "C" fn(ud: *mut c_void, data: *mut c_void, len: u64, cmd: c_int) -> i64>;

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

/// A single extra-field mutation, applied in order to an entry's extra fields.
#[derive(Debug, Clone)]
enum ExtraFieldOp {
    /// Add/replace the field with `id` at `ef_idx` (or append if `ZIP_EXTRA_FIELD_NEW`).
    Set { id: u16, ef_idx: u16, data: Vec<u8> },
    /// Remove the `ef_idx`-th extra field (or all if `ZIP_EXTRA_FIELD_ALL`).
    Delete { ef_idx: u16 },
    /// Remove all extra fields with `id` (or all if `id` is `ZIP_EXTRA_FIELD_ALL`),
    /// or the `ef_idx`-th field with that `id` (`ZIP_EXTRA_FIELD_ALL` = all).
    DeleteById { id: u16, ef_idx: u16 },
}

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
    // ---- Phase 3: write-path metadata ----
    /// Per-entry extra-field mutations `(index, op)`, applied in order.
    extra_field_ops: Vec<(u64, ExtraFieldOp)>,
    /// Per-entry file comment changes `(index, Option<bytes>)` (`None` = remove).
    file_comments: Vec<(u64, Option<Vec<u8>>)>,
    /// Per-entry DOS timestamps `(index, dos_time, dos_date)`.
    dostimes: Vec<(u64, u16, u16)>,
    /// Per-entry mtime changes `(index, unix_seconds)`.
    mtimes: Vec<(u64, i64)>,
    /// Per-entry external attributes `(index, opsys, attributes)`.
    ext_attrs: Vec<(u64, u8, u32)>,
    /// Per-entry compression `(index, method_i32, level)`.
    compressions: Vec<(u64, i32, u32)>,
    /// Pending archive (EOCD) comment. `Some` = set (empty = none); `None` = no change.
    archive_comment: Option<Vec<u8>>,
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
            && self.extra_field_ops.is_empty()
            && self.file_comments.is_empty()
            && self.dostimes.is_empty()
            && self.mtimes.is_empty()
            && self.ext_attrs.is_empty()
            && self.compressions.is_empty()
            && self.archive_comment.is_none()
    }
}

/// The Rust state behind an opaque `zip_source_t*`.
///
/// This is the public layered-source model (Phase 4): every `zip_source_*`
/// API returns one of these handles, and each is a node in a stack of layers
/// (buffer / file / window / zip-entry / user-function). Internally a source
/// is either an in-memory byte provider (`Memory`) or a user-supplied C
/// callback (`Function`); all access is serialized by a `Mutex` so the handle
/// can be shared across threads. Command dispatch mirrors libzip's
/// `_zip_source_call` / `zip_source_cmd` switch, and `catch_unwind` at the ABI
/// boundary keeps malformed callbacks from aborting the host process.
struct ZipSource {
    /// Command-dispatch backend.
    backend: Mutex<SourceBackend>,
    /// Source error object (returned by `zip_source_error`).
    error: Mutex<ZipErrorState>,
    /// Reference count (`zip_source_keep` / `zip_source_free`).
    refcount: AtomicI32,
    /// Cached command-support bitmask. For `Memory` sources it is fixed at
    /// creation; for `Function` sources it is queried lazily via `SUPPORTS`.
    supports: Mutex<i64>,
    /// Whether the source has been `OPEN`ed (read is permitted).
    opened: Mutex<bool>,
    /// Set once `READ` reaches EOF (used by `zip_source_at_eof`).
    eof: AtomicI32,
}

/// The byte-provider behind a [`ZipSource`].
#[allow(dead_code)] // `Write`/layered variants are Phase 4b.
enum SourceBackend {
    /// An in-memory byte buffer (already windowed/sliced). Seekable, readable.
    Memory {
        data: Vec<u8>,
        pos: u64,
        /// Metadata reported by `zip_source_stat`.
        stat: Stat,
    },
    /// A user-supplied C callback source (function source).
    Function {
        cb: ZipSourceCallback,
        ud: *mut c_void,
    },
}

// The backend may hold a raw C function pointer / userdata pointer. All
// dereferencing happens on the C side (or through the callback), guarded by
// the Mutex, so sharing the handle across threads is as safe as libzip's own
// refcounted source sharing.
unsafe impl Send for ZipSource {}
unsafe impl Sync for ZipSource {}

impl ZipSource {
    fn new(backend: SourceBackend, supports: i64) -> ZipSource {
        ZipSource {
            backend: Mutex::new(backend),
            error: Mutex::new(ZipErrorState {
                ze: zip_error {
                    zip_err: 0,
                    sys_err: 0,
                    str: std::ptr::null_mut(),
                },
                owned: Some(CString::new(err_str(0)).unwrap_or_default()),
            }),
            refcount: AtomicI32::new(1),
            supports: Mutex::new(supports),
            opened: Mutex::new(false),
            eof: AtomicI32::new(0),
        }
    }

    fn set_err(&self, code: i32, sys: i32) {
        let mut g = self.error.lock().unwrap_or_else(|e| e.into_inner());
        g.ze.zip_err = code;
        g.ze.sys_err = sys;
        let s = CString::new(err_str(code)).unwrap_or_default();
        g.ze.str = s.as_ptr() as *mut c_char;
        g.owned = Some(s);
    }

    /// Read the source's full content (open, drain, close). Used by
    /// `zip_file_add`/`zip_file_replace` and by the window/zip layered
    /// constructors to materialize a sub-range.
    fn read_all(&self) -> Result<Vec<u8>, i32> {
        let opened = self.dispatch(ZIP_SOURCE_OPEN, std::ptr::null_mut(), 0);
        if opened < 0 {
            return Err(-1);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = self.dispatch(
                ZIP_SOURCE_READ,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u64,
            );
            if n < 0 {
                self.dispatch(ZIP_SOURCE_CLOSE, std::ptr::null_mut(), 0);
                return Err(-1);
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        self.dispatch(ZIP_SOURCE_CLOSE, std::ptr::null_mut(), 0);
        Ok(out)
    }

    /// Dispatch a single `zip_source_cmd` to the backend, mirroring libzip's
    /// `_zip_source_call`. Panics from a user callback are caught by the
    /// `guarded()` wrapper at the ABI boundary.
    fn dispatch(&self, cmd: c_int, data: *mut c_void, len: u64) -> i64 {
        let mut b = self.backend.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *b {
            SourceBackend::Memory {
                data: bytes,
                pos,
                stat,
            } => match cmd {
                ZIP_SOURCE_OPEN => {
                    *pos = 0;
                    *self.opened.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    self.eof.store(0, Ordering::Relaxed);
                    0
                }
                ZIP_SOURCE_CLOSE => 0,
                ZIP_SOURCE_READ => {
                    let remaining = (bytes.len() as u64).saturating_sub(*pos);
                    let n = remaining.min(len) as usize;
                    if n > 0 {
                        let out =
                            unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), n) };
                        out.copy_from_slice(&bytes[*pos as usize..*pos as usize + n]);
                        *pos += n as u64;
                    }
                    if n == 0 && *pos >= bytes.len() as u64 {
                        self.eof.store(1, Ordering::Relaxed);
                    }
                    n as i64
                }
                ZIP_SOURCE_SEEK => {
                    if data.is_null() || len < std::mem::size_of::<ZipSourceArgsSeek>() as u64 {
                        self.set_err(ZipErrorCode::Inval.as_i32(), 0);
                        return -1;
                    }
                    let args = unsafe { &*(data.cast::<ZipSourceArgsSeek>()) };
                    let size = bytes.len() as i64;
                    let target: i64 = match args.whence {
                        SEEK_SET => args.offset,
                        SEEK_CUR => *pos as i64 + args.offset,
                        SEEK_END => size + args.offset,
                        _ => {
                            self.set_err(ZipErrorCode::Inval.as_i32(), 0);
                            return -1;
                        }
                    };
                    if target < 0 {
                        self.set_err(ZipErrorCode::Inval.as_i32(), 0);
                        return -1;
                    }
                    *pos = (target as u64).min(bytes.len() as u64);
                    if *pos >= bytes.len() as u64 {
                        self.eof.store(1, Ordering::Relaxed);
                    } else {
                        self.eof.store(0, Ordering::Relaxed);
                    }
                    0
                }
                ZIP_SOURCE_TELL => *pos as i64,
                ZIP_SOURCE_STAT => {
                    if data.is_null() || len < std::mem::size_of::<zip_stat>() as u64 {
                        self.set_err(ZipErrorCode::Inval.as_i32(), 0);
                        return -1;
                    }
                    let sb = unsafe { &mut *(data.cast::<zip_stat>()) };
                    fill_zip_stat(sb, stat);
                    0
                }
                ZIP_SOURCE_ERROR => {
                    let g = self.error.lock().unwrap_or_else(|e| e.into_inner());
                    if data.is_null() || len < 2 * std::mem::size_of::<c_int>() as u64 {
                        return -1;
                    }
                    let out = unsafe { &mut *(data.cast::<[c_int; 2]>()) };
                    out[0] = g.ze.zip_err;
                    out[1] = g.ze.sys_err;
                    0
                }
                ZIP_SOURCE_SUPPORTS => *self.supports.lock().unwrap_or_else(|e| e.into_inner()),
                ZIP_SOURCE_AT_EOF => {
                    if *pos >= bytes.len() as u64 {
                        1
                    } else {
                        0
                    }
                }
                ZIP_SOURCE_FREE => 0,
                _ => {
                    self.set_err(ZipErrorCode::Opnotsupp.as_i32(), 0);
                    -1
                }
            },
            SourceBackend::Function { cb, ud } => match cb {
                Some(cb) => {
                    let r = unsafe { cb(*ud, data, len, cmd) };
                    // Track EOF for `zip_source_at_eof` when READ returns 0.
                    if cmd == ZIP_SOURCE_READ && r == 0 {
                        self.eof.store(1, Ordering::Relaxed);
                    }
                    r
                }
                None => {
                    self.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    -1
                }
            },
        }
    }

    /// Cache the command-support mask (used by `is_seekable`/`is_deleted`).
    fn query_supports(&self) -> i64 {
        let r = self.dispatch(ZIP_SOURCE_SUPPORTS, std::ptr::null_mut(), 0);
        if r < 0 {
            // Fall back to the creation-time mask.
            return *self.supports.lock().unwrap_or_else(|e| e.into_inner());
        }
        *self.supports.lock().unwrap_or_else(|e| e.into_inner()) = r;
        r
    }
}

/// Fill a C `zip_stat` from an internal [`Stat`], mapping the internal `valid`
/// bitmask onto the `ZIP_STAT_*` bits libzip uses.
fn fill_zip_stat(sb: &mut zip_stat, s: &Stat) {
    let mut valid: u64 = 0;
    if s.size.is_some() {
        valid |= ZIP_STAT_SIZE;
    }
    if s.comp_size.is_some() {
        valid |= ZIP_STAT_COMP_SIZE;
    }
    if s.mtime.is_some() {
        valid |= ZIP_STAT_MTIME;
    }
    if s.crc.is_some() {
        valid |= ZIP_STAT_CRC;
    }
    if s.comp_method.is_some() {
        valid |= ZIP_STAT_COMP_METHOD;
    }
    if s.encryption_method.is_some() {
        valid |= ZIP_STAT_ENCRYPTION_METHOD;
    }
    // Source stats (unlike `zip_stat` on an archive entry) do not carry a
    // NAME/INDEX; libzip leaves those fields at their defaults for sources.
    sb.valid = valid;
    sb.name = std::ptr::null();
    sb.index = 0;
    sb.size = s.size.unwrap_or(0);
    sb.comp_size = s.comp_size.unwrap_or(0);
    sb.mtime = s.mtime.unwrap_or(0) as i64;
    sb.crc = s.crc.unwrap_or(0);
    sb.comp_method = s.comp_method.unwrap_or(0);
    sb.encryption_method = s.encryption_method.unwrap_or(0);
    sb.flags = 0;
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

/// `zip_file_attributes_t` layout, mirroring libzip's `struct zip_file_attributes`
/// (used by `zip_file_get_external_attributes` / `zip_file_attributes_init`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct zip_file_attributes {
    pub valid: u64,
    pub version: u8,
    pub host_system: u8,
    pub ascii: u8,
    pub version_needed: u8,
    pub external_file_attributes: u32,
    pub general_purpose_bit_flags: u16,
    pub general_purpose_bit_mask: u16,
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
    /// Wrapped in a `Mutex` so the setters can update the overlay that
    /// `zip_file_get_comment` reflects (stable pointers until close).
    comments: Mutex<Vec<Option<CString>>>,
    /// Archive (EOCD) comment as raw bytes **plus a trailing NUL** (so the
    /// getter can return a stable C-string pointer). The raw comment length
    /// (including any trailing NUL a C caller passed to the setter, matching
    /// libzip) is `len() - 1`; the getter reports exactly that length. Stored as
    /// raw `Vec<u8>` rather than `CString` because a caller may pass the
    /// NUL-terminated string length (e.g. `"comment"` = 8 chars with len 9),
    /// which embeds a NUL that `CString` would reject.
    archive_comment: Mutex<Option<Vec<u8>>>,
    /// Cached effective extra fields per logical entry index (stable pointers
    /// for `zip_file_extra_field_get*`; valid until the next metadata change).
    /// Internal ZIP64/AES fields are excluded.
    extra_fields: Mutex<Vec<Vec<(u16, Vec<u8>)>>>,
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

    /// Validate `index` against existing entries plus pending adds.
    fn valid_index(&self, index: u64) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        index < self.archive.len() + pending.adds.len() as u64
    }

    /// Effective file comment for the entry at `index`: the last pending
    /// `file_comments` change wins, else the original entry comment.
    fn effective_comment(&self, pending: &PendingOps, index: u64) -> Option<Vec<u8>> {
        for &(idx, ref cmt) in pending.file_comments.iter().rev() {
            if idx == index {
                return cmt.clone();
            }
        }
        self.archive.dirent(index).and_then(|d| {
            if d.comment.is_empty() {
                None
            } else {
                Some(d.comment.as_bytes().to_vec())
            }
        })
    }

    /// Effective `(opsys, external_attributes)` for the entry at `index`.
    fn effective_ext_attrs(&self, pending: &PendingOps, index: u64) -> Option<(u8, u32)> {
        for &(idx, opsys, attrs) in pending.ext_attrs.iter().rev() {
            if idx == index {
                return Some((opsys, attrs));
            }
        }
        self.archive.dirent(index).map(|d| {
            let opsys = (d.version_madeby >> 8) as u8;
            (opsys, d.ext_attrib)
        })
    }

    /// Effective DOS `(time, date)` for the entry at `index`: a pending mtime
    /// is converted via `unix_to_dos`, else a pending dostime, else original.
    fn effective_dostime(&self, pending: &PendingOps, index: u64) -> (u16, u16) {
        if let Some(&(_, ut)) = pending.mtimes.iter().rev().find(|(i, _)| *i == index) {
            return zip_core::archive::unix_to_dos(ut.max(0) as u64);
        }
        if let Some(&(_, t, d)) = pending.dostimes.iter().rev().find(|(i, _, _)| *i == index) {
            return (t, d);
        }
        match self.archive.dirent(index) {
            Some(d) => (d.last_mod_time, d.last_mod_date),
            None => (0, 0),
        }
    }

    /// Ensure the extra-fields overlay has a slot for `index`.
    fn ensure_extra_slot(&self, index: u64) {
        let mut v = self.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
        if (index as usize) >= v.len() {
            v.resize(index as usize + 1, Vec::new());
        }
    }

    /// Recompute the cached extra-field overlay for `index` from the ordered
    /// pending ops (plus the original entry fields), so getters reflect pending
    /// set/delete changes immediately.
    fn refresh_extra_fields(&self, pending: &PendingOps, index: u64) {
        let mut base: Vec<(u16, Vec<u8>)> = match self.archive.dirent(index) {
            Some(d) => d
                .extra_fields
                .iter()
                .filter(|(id, _)| *id != ZIP_EF_ZIP64 && *id != ZIP_EF_WINZIP_AES)
                .map(|(id, data)| (*id, data.clone()))
                .collect(),
            None => Vec::new(),
        };
        for &(idx, ref op) in &pending.extra_field_ops {
            if idx != index {
                continue;
            }
            match op {
                ExtraFieldOp::Delete { ef_idx } => {
                    if *ef_idx == ZIP_EXTRA_FIELD_ALL {
                        base.clear();
                    } else if (*ef_idx as usize) < base.len() {
                        base.remove(*ef_idx as usize);
                    }
                }
                ExtraFieldOp::DeleteById { id, ef_idx } => {
                    if *id == ZIP_EXTRA_FIELD_ALL || *ef_idx == ZIP_EXTRA_FIELD_ALL {
                        base.retain(|(fid, _)| *fid != *id);
                    } else {
                        // Remove the `ef_idx`-th field with matching `id`.
                        let mut n = 0u16;
                        let mut removed = false;
                        let mut kept = Vec::with_capacity(base.len());
                        for f in base.drain(..) {
                            if f.0 == *id && !removed {
                                if n == *ef_idx {
                                    removed = true;
                                    continue;
                                }
                                n += 1;
                            }
                            kept.push(f);
                        }
                        base = kept;
                    }
                }
                ExtraFieldOp::Set { id, ef_idx, data } => {
                    if *ef_idx == ZIP_EXTRA_FIELD_NEW {
                        base.push((*id, data.clone()));
                    } else {
                        let mut n = 0u16;
                        let mut replaced = false;
                        for f in base.iter_mut() {
                            if f.0 == *id {
                                if n == *ef_idx {
                                    *f = (*id, data.clone());
                                    replaced = true;
                                    break;
                                }
                                n += 1;
                            }
                        }
                        if !replaced {
                            base.push((*id, data.clone()));
                        }
                    }
                }
            }
        }
        // `pending` is borrowed from the caller; release the extra-fields
        // cache lock only after the overlay has been fully recomputed.
        let mut v = self.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
        if (index as usize) >= v.len() {
            v.resize(index as usize + 1, Vec::new());
        }
        v[index as usize] = base;
    }

    /// Effective extra fields for `index` for use by materialize (reads the
    /// cached overlay, refreshing it if it is stale relative to pending ops).
    fn effective_extra_fields(&self, pending: &PendingOps, index: u64) -> Vec<(u16, Vec<u8>)> {
        self.refresh_extra_fields(pending, index);
        let v = self.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
        v.get(index as usize).cloned().unwrap_or_default()
    }

    /// Record a pending file-comment change and update the getter overlay.
    fn set_file_comment_pending(&self, index: u64, bytes: Option<Vec<u8>>) -> Result<(), i32> {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.file_comments.retain(|(i, _)| *i != index);
            pending.file_comments.push((index, bytes.clone()));
        }
        let mut cvec = self.comments.lock().unwrap_or_else(|e| e.into_inner());
        if (index as usize) >= cvec.len() {
            cvec.resize(index as usize + 1, None);
        }
        cvec[index as usize] = match bytes {
            None => None,
            Some(b) if b.is_empty() => None,
            Some(b) => CString::new(b).ok(),
        };
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
                    .filter(|d| !d.comment.is_empty())
                    .and_then(|d| CString::new(d.comment.as_str()).ok())
            })
            .collect::<Vec<_>>();
        let archive_comment = {
            let c = archive.comment();
            if c.is_empty() {
                None
            } else {
                let mut buf = c.as_bytes().to_vec();
                buf.push(0);
                Some(buf)
            }
        };
        // Effective extra fields per entry (excluding internal ZIP64/AES fields
        // the writer manages itself), so getters return stable pointers.
        let extra_fields = (0..archive.len())
            .map(|i| {
                archive
                    .dirent(i)
                    .map(|d| {
                        d.extra_fields
                            .iter()
                            .filter(|(id, _)| *id != ZIP_EF_ZIP64 && *id != ZIP_EF_WINZIP_AES)
                            .map(|(id, data)| (*id, data.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let z = Box::new(Zip {
            archive,
            names,
            comments: Mutex::new(comments),
            archive_comment: Mutex::new(archive_comment),
            extra_fields: Mutex::new(extra_fields),
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
/// stream, carrying Phase 3 write-path metadata (comments, extra fields, DOS
/// timestamps, external attributes, per-entry compression) and the archive
/// comment.
fn materialize(z: &Zip) -> Result<Vec<u8>, i32> {
    let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
    // Logical index -> (file, encryption method). Logical indices match those
    // returned by `zip_file_add` / the original entry indices used by
    // `zip_file_set_encryption`.
    let mut files: Vec<ArchiveFile> = Vec::new();
    let mut methods: Vec<u16> = Vec::new();

    // Populate per-entry metadata onto `f` for logical entry `index`.
    fn apply_meta(z: &Zip, pending: &PendingOps, f: &mut ArchiveFile, index: u64) {
        f.comment = z.effective_comment(pending, index);
        f.extra_fields = z.effective_extra_fields(pending, index);
        let (t, d) = z.effective_dostime(pending, index);
        f.last_mod_time = t;
        f.last_mod_date = d;
        if let Some((opsys, attrs)) = z.effective_ext_attrs(pending, index) {
            f.opsys = opsys;
            f.external_attributes = attrs;
        }
        if let Some(&(_, method, level)) = pending
            .compressions
            .iter()
            .find(|(idx, _, _)| *idx == index)
        {
            if let Some(cm) = compression_from_i32(method) {
                f.method = Some(cm);
                f.level = Some(level);
            }
        }
    }

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
        let mut f = ArchiveFile::new(name, data);
        apply_meta(z, &pending, &mut f, i);
        files.push(f);
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
        let mut f = ArchiveFile::new(add.name.clone(), add.data.clone());
        apply_meta(z, &pending, &mut f, logical);
        files.push(f);
        let method = pending
            .encryptions
            .iter()
            .find(|(idx, _)| *idx == logical)
            .map(|(_, m)| *m)
            .unwrap_or(0);
        methods.push(method);
    }

    let archive_comment = match &pending.archive_comment {
        Some(c) => c.clone(),
        None => z.archive.comment().as_bytes().to_vec(),
    };

    let any_enc = methods.iter().any(|&m| m != 0);
    let pw: Vec<u8> = if any_enc {
        z.password
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(ZipErrorCode::Nopasswd.as_i32())?
    } else {
        Vec::new()
    };
    write_archive_full(
        &files,
        &CompressOptions::default(),
        &pw,
        &methods,
        &archive_comment,
    )
    .map_err(|e| e.code().as_i32())
}

/// Map a libzip compression-method integer to a `CompressionMethod` for writing.
/// `ZIP_CM_DEFAULT` (-1) means "no override" and returns `None`.
fn compression_from_i32(m: i32) -> Option<CompressionMethod> {
    match m {
        ZIP_CM_DEFAULT => None,
        ZIP_CM_STORE => Some(CompressionMethod::Store),
        ZIP_CM_DEFLATE => Some(CompressionMethod::Deflate),
        _ => None,
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
        (*sb).mtime = {
            let idx = stat.index.unwrap_or(0);
            let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            let (t, d) = z.effective_dostime(&pending, idx);
            zip_core::archive::dos_to_unix(t, d) as i64
        };
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
            let src = Box::new(ZipSource::new(
                SourceBackend::Memory {
                    data: slice.to_vec(),
                    pos: 0,
                    stat: Stat {
                        size: Some(len),
                        valid: ZIP_STAT_SIZE,
                        ..Stat::default()
                    },
                },
                SRC_SEEKABLE | src_bit(ZIP_SOURCE_AT_EOF),
            ));
            Ok(Box::into_raw(src) as H)
        },
        std::ptr::null_mut(),
    )
}

/// Release a source created by [`zip_source_buffer`] (or any `zip_source_*`
/// constructor), decrementing its reference count and freeing it when it
/// reaches zero (as `zip_source_keep` may have bumped it).
///
/// # Safety
///
/// `source` must be a handle returned by a `zip_source_*` constructor not
/// already freed (or kept past its final free).
#[no_mangle]
pub unsafe extern "C" fn zip_source_free(source: H) {
    if source.is_null() {
        return;
    }
    let s = source.cast::<ZipSource>().as_ref();
    if let Some(s) = s {
        let old = s.refcount.fetch_sub(1, Ordering::Relaxed);
        if old <= 1 {
            drop(Box::from_raw(source.cast::<ZipSource>()));
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 4a: streaming zip_source_* core (file / function / window / zip)
// ---------------------------------------------------------------------------

/// Build the memory backend for a source from `data` with an explicit `stat`.
/// `start`/`len` are applied as a window (`len == -1` means "to EOF").
fn make_memory_source(data: Vec<u8>, start: u64, len: i64, mut stat: Stat) -> Option<ZipSource> {
    let dlen = data.len() as u64;
    let (lo, hi) = if len == ZIP_LENGTH_UNCHECKED {
        (start, dlen)
    } else {
        let hi = if len < 0 {
            dlen
        } else {
            (start + len as u64).min(dlen)
        };
        (start, hi)
    };
    if lo > hi || lo > dlen {
        return None;
    }
    stat.size = Some(hi - lo);
    stat.valid |= ZIP_STAT_SIZE;
    let slice = data[lo as usize..hi as usize].to_vec();
    Some(ZipSource::new(
        SourceBackend::Memory {
            data: slice,
            pos: 0,
            stat,
        },
        SRC_SEEKABLE | src_bit(ZIP_SOURCE_AT_EOF),
    ))
}

/// Create a `zip_source_t*` over the file at `path`, exposing the sub-range
/// `[start, start+len)` (`len == -1` means the rest of the file).
///
/// `zip_source_file_create` is the `_create` form (error via `errorp`);
/// `zip_source_file` is the archive form (error on the archive handle).
unsafe fn source_file_from_path(
    zh: H,
    path: *const c_char,
    start: u64,
    len: i64,
    errorp: *mut c_int,
) -> H {
    catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if path.is_null() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        let p = CStr::from_ptr(path).to_str().map_err(|_| ZipErrorCode::Inval.as_i32())?;
        let meta = std::fs::metadata(p).map_err(|_| ZipErrorCode::Open.as_i32())?;
        if !meta.is_file() {
            return Err(ZipErrorCode::Open.as_i32());
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let data = std::fs::read(p).map_err(|_| ZipErrorCode::Read.as_i32())?;
        let stat = Stat {
            size: Some(data.len() as u64),
            mtime,
            valid: ZIP_STAT_SIZE | ZIP_STAT_MTIME,
            ..Stat::default()
        };
        let src = make_memory_source(data, start, len, stat).ok_or(ZipErrorCode::Inval.as_i32())?;
        // The `_create`/`zip_source_file` split only affects where the error is
        // reported; the source itself is identical.
        let _ = zh;
        let _ = errorp;
        Ok(Box::into_raw(Box::new(src)) as H)
    }))
        .map_or(std::ptr::null_mut(), |r| r.unwrap_or(std::ptr::null_mut()))
}

/// `zip_source_file_create(path, start, len, errorp)`.
///
/// # Safety
///
/// `path` a NUL-terminated C string; `errorp` (if non-null) writable `int`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_file_create(
    path: *const c_char,
    start: u64,
    len: i64,
    errorp: *mut c_int,
) -> H {
    let h = source_file_from_path(std::ptr::null_mut(), path, start, len, errorp);
    if h.is_null() && !errorp.is_null() {
        unsafe {
            *errorp = ZipErrorCode::Open.as_i32();
        }
    }
    h
}

/// `zip_source_file(zh, path, start, len)` — archive form of
/// [`zip_source_file_create`]; errors are set on the archive handle.
///
/// # Safety
///
/// `zh` a valid handle; `path` a NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn zip_source_file(
    zh: H,
    path: *const c_char,
    start: u64,
    len: i64,
) -> H {
    let h = source_file_from_path(zh, path, start, len, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Open.as_i32(), 0);
        }
    }
    h
}

/// Create a `zip_source_t*` over an open `FILE*`, exposing `[start, start+len)`.
///
/// # Safety
///
/// `file` a valid open `FILE*`; `errorp` (if non-null) writable `int`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_filep_create(
    file: *mut libc::FILE,
    start: u64,
    len: i64,
    errorp: *mut c_int,
) -> H {
    let h = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if file.is_null() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        // Determine the file size by seeking to the end, then restore the
        // position to the start so we can read the whole content.
        let cur = unsafe { libc::ftell(file) };
        if unsafe { libc::fseek(file, 0, SEEK_END) } != 0 {
            return Err(ZipErrorCode::Seek.as_i32());
        }
        let size = unsafe { libc::ftell(file) };
        if unsafe { libc::fseek(file, 0, SEEK_SET) } != 0 {
            return Err(ZipErrorCode::Seek.as_i32());
        }
        if size < 0 {
            return Err(ZipErrorCode::Seek.as_i32());
        }
        let mut data = vec![0u8; size as usize];
        if size > 0 {
            let n = unsafe { libc::fread(data.as_mut_ptr().cast(), 1, size as usize, file) };
            if n != size as usize {
                return Err(ZipErrorCode::Read.as_i32());
            }
        }
        // Restore the caller's position.
        if cur >= 0 {
            unsafe {
                libc::fseek(file, cur, SEEK_SET);
            }
        }
        let stat = Stat {
            size: Some(data.len() as u64),
            valid: ZIP_STAT_SIZE | ZIP_STAT_MTIME,
            ..Stat::default()
        };
        let src = make_memory_source(data, start, len, stat).ok_or(ZipErrorCode::Inval.as_i32())?;
        Ok(Box::into_raw(Box::new(src)) as H)
    }))
        .map_or(std::ptr::null_mut(), |r| r.unwrap_or(std::ptr::null_mut()));
    if h.is_null() && !errorp.is_null() {
        unsafe {
            *errorp = ZipErrorCode::Open.as_i32();
        }
    }
    h
}

/// `zip_source_filep(zh, file, start, len)` — archive form of
/// [`zip_source_filep_create`]; errors set on the archive handle.
///
/// # Safety
///
/// `zh` a valid handle; `file` a valid open `FILE*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_filep(
    zh: H,
    file: *mut libc::FILE,
    start: u64,
    len: i64,
) -> H {
    let h = zip_source_filep_create(file, start, len, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Open.as_i32(), 0);
        }
    }
    h
}

/// Create a `zip_source_t*` over a user-supplied C callback.
///
/// # Safety
///
/// `cb` a valid `zip_source_callback`; `ud` is passed back to `cb` unchanged.
#[no_mangle]
pub unsafe extern "C" fn zip_source_function_create(
    cb: ZipSourceCallback,
    ud: *mut c_void,
    errorp: *mut c_int,
) -> H {
    let h = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if cb.is_none() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        let src = ZipSource::new(
            SourceBackend::Function { cb, ud },
            // Default to readable+seekable; refined by the first SUPPORTS query.
            SRC_SEEKABLE,
        );
        Ok(Box::into_raw(Box::new(src)) as H)
    }))
        .map_or(std::ptr::null_mut(), |r| r.unwrap_or(std::ptr::null_mut()));
    if h.is_null() && !errorp.is_null() {
        unsafe {
            *errorp = ZipErrorCode::Inval.as_i32();
        }
    }
    h
}

/// `zip_source_function(zh, cb, ud)` — archive form of
/// [`zip_source_function_create`]; errors set on the archive handle.
///
/// # Safety
///
/// `zh` a valid handle; `cb` a valid `zip_source_callback`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_function(
    zh: H,
    cb: ZipSourceCallback,
    ud: *mut c_void,
) -> H {
    let h = zip_source_function_create(cb, ud, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Inval.as_i32(), 0);
        }
    }
    h
}

/// Create a window source over `src`, exposing `[offset, offset+len)`.
/// Takes ownership of `src` (frees it when the window is freed).
///
/// # Safety
///
/// `src` a valid `zip_source_t*`; the caller must not use it afterwards.
#[no_mangle]
pub unsafe extern "C" fn zip_source_window_create(
    src: H,
    offset: u64,
    len: i64,
    errorp: *mut c_int,
) -> H {
    let h = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        if src.is_null() {
            return Err(ZipErrorCode::Inval.as_i32());
        }
        let s = src.cast::<ZipSource>().as_ref().ok_or(ZipErrorCode::Inval.as_i32())?;
        // Capture the lower layer's stat (mtime etc.) via a proper `zip_stat`
        // buffer before consuming it.
        let mut zs = zip_stat {
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
        let stat_ok = s.dispatch(
            ZIP_SOURCE_STAT,
            (&mut zs) as *mut zip_stat as *mut c_void,
            std::mem::size_of::<zip_stat>() as u64,
        );
        let data = s.read_all().map_err(|_| ZipErrorCode::Read.as_i32())?;
        // The window takes ownership: free the lower source now.
        zip_source_free(src);
        let mut st = Stat::default();
        if stat_ok >= 0 && zs.valid & ZIP_STAT_MTIME != 0 {
            st.mtime = Some(zs.mtime as u64);
        }
        let src = make_memory_source(data, offset, len, st).ok_or(ZipErrorCode::Inval.as_i32())?;
        Ok(Box::into_raw(Box::new(src)) as H)
    }))
        .map_or(std::ptr::null_mut(), |r| r.unwrap_or(std::ptr::null_mut()));
    if h.is_null() && !errorp.is_null() {
        unsafe {
            *errorp = ZipErrorCode::Inval.as_i32();
        }
    }
    h
}

/// `zip_source_window(zh, src, offset, len)` — archive form of
/// [`zip_source_window_create`]; errors set on the archive handle.
///
/// # Safety
///
/// `zh` a valid handle; `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_window(
    zh: H,
    src: H,
    offset: u64,
    len: i64,
) -> H {
    let h = zip_source_window_create(src, offset, len, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = zh.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Inval.as_i32(), 0);
        }
    }
    h
}

/// Create a source over entry `index` of archive `za`, exposing its content.
/// With `ZIP_FL_COMPRESSED` the raw compressed (stored) bytes are exposed;
/// otherwise the decompressed content is used. `[start, start+len)` selects a
/// sub-range (`len == -1` = rest of entry).
///
/// # Safety
///
/// `za` a valid archive handle; `errorp` (if non-null) writable `int`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_zip_create(
    za: H,
    index: u64,
    flags: u32,
    start: u64,
    len: i64,
    errorp: *mut c_int,
) -> H {
    let h = catch_unwind(AssertUnwindSafe(|| -> Result<H, i32> {
        let z = za.cast::<Zip>().as_ref().ok_or(ZipErrorCode::Inval.as_i32())?;
        let compressed = flags & ZIP_FL_COMPRESSED != 0;
        let data = if compressed {
            z.archive.read_compressed_entry(index).map_err(|e| e.code().as_i32())?
        } else {
            z.archive.read_entry(index).map_err(|e| e.code().as_i32())?
        };
        let st = z.archive.stat(index).unwrap_or_default();
        let src = make_memory_source(data, start, len, st).ok_or(ZipErrorCode::Inval.as_i32())?;
        Ok(Box::into_raw(Box::new(src)) as H)
    }))
        .map_or(std::ptr::null_mut(), |r| r.unwrap_or(std::ptr::null_mut()));
    if h.is_null() && !errorp.is_null() {
        // Recover the failure code from the archive's error if available.
        let code = za
            .cast::<Zip>()
            .as_ref()
            .and_then(|z| z.error.lock().ok())
            .map(|g| g.ze.zip_err)
            .unwrap_or(ZipErrorCode::Read.as_i32());
        unsafe {
            *errorp = code;
        }
    }
    h
}

/// `zip_source_zip(za, srcza, index, flags, start, len)` — deprecated archive
/// form of [`zip_source_zip_create`]; errors set on `za`.
///
/// # Safety
///
/// `za`, `srcza` valid archive handles.
#[no_mangle]
pub unsafe extern "C" fn zip_source_zip(
    za: H,
    srcza: H,
    index: u64,
    flags: u32,
    start: u64,
    len: i64,
) -> H {
    let h = zip_source_zip_create(srcza, index, flags, start, len, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = za.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Read.as_i32(), 0);
        }
    }
    h
}

/// `zip_source_zip_file_create(za, index, flags, start, len, name, errorp)`.
/// `name` is accepted for ABI compatibility (libzip uses it to select a
/// replacement name); the content is read by `index` regardless.
///
/// # Safety
///
/// `za` a valid archive handle; `errorp` (if non-null) writable `int`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_zip_file_create(
    za: H,
    index: u64,
    flags: u32,
    start: u64,
    len: i64,
    _name: *const c_char,
    errorp: *mut c_int,
) -> H {
    zip_source_zip_create(za, index, flags, start, len, errorp)
}

/// `zip_source_zip_file(za, srcza, index, flags, start, len, name)`.
///
/// # Safety
///
/// `za`, `srcza` valid archive handles.
#[no_mangle]
pub unsafe extern "C" fn zip_source_zip_file(
    za: H,
    srcza: H,
    index: u64,
    flags: u32,
    start: u64,
    len: i64,
    _name: *const c_char,
) -> H {
    let h = zip_source_zip_create(srcza, index, flags, start, len, std::ptr::null_mut());
    if h.is_null() {
        if let Some(z) = za.cast::<Zip>().as_ref() {
            z.set_err(ZipErrorCode::Read.as_i32(), 0);
        }
    }
    h
}

// ---- source read/write/seek/stat primitives --------------------------------

/// `zip_source_open(src)` — send `ZIP_SOURCE_OPEN`. Returns 0 or -1.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_open(src: H) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(ZIP_SOURCE_OPEN, std::ptr::null_mut(), 0);
            if r < 0 {
                s.set_err(ZipErrorCode::Opnotsupp.as_i32(), 0);
                Err(-1)
            } else {
                Ok(0)
            }
        },
        -1,
    )
}

/// `zip_source_close(src)` — send `ZIP_SOURCE_CLOSE`. Returns 0 or -1.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_close(src: H) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(ZIP_SOURCE_CLOSE, std::ptr::null_mut(), 0);
            if r < 0 {
                s.set_err(ZipErrorCode::Opnotsupp.as_i32(), 0);
                Err(-1)
            } else {
                Ok(0)
            }
        },
        -1,
    )
}

/// `zip_source_read(src, buf, len)` — read up to `len` bytes. Returns bytes
/// read, 0 at EOF, or -1 on error.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`; `buf` a writable buffer of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_source_read(src: H, buf: *mut c_void, len: u64) -> i64 {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(ZIP_SOURCE_READ, buf, len);
            if r < 0 {
                s.set_err(ZipErrorCode::Read.as_i32(), 0);
            }
            Ok(r)
        },
        -1,
    )
}

/// `zip_source_seek(src, offset, whence)` — seek within the source. Returns 0
/// or -1.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_seek(src: H, offset: i64, whence: c_int) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let args = ZipSourceArgsSeek { offset, whence };
            let r = s.dispatch(
                ZIP_SOURCE_SEEK,
                (&args) as *const ZipSourceArgsSeek as *mut c_void,
                std::mem::size_of::<ZipSourceArgsSeek>() as u64,
            );
            if r < 0 {
                Err(-1)
            } else {
                Ok(0)
            }
        },
        -1,
    )
}

/// `zip_source_tell(src)` — return the current read position, or -1 on error.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_tell(src: H) -> i64 {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            Ok(s.dispatch(ZIP_SOURCE_TELL, std::ptr::null_mut(), 0))
        },
        -1,
    )
}

/// `zip_source_stat(src, sb)` — fill `sb` with the source's metadata. Returns 0
/// or -1.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`; `sb` a valid `zip_stat_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_stat(src: H, sb: *mut zip_stat) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(
                ZIP_SOURCE_STAT,
                sb as *mut c_void,
                std::mem::size_of::<zip_stat>() as u64,
            );
            if r < 0 {
                Err(-1)
            } else {
                Ok(0)
            }
        },
        -1,
    )
}

/// `zip_source_at_eof(src)` — return 1 if the source is at EOF, 0 otherwise.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_at_eof(src: H) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(ZIP_SOURCE_AT_EOF, std::ptr::null_mut(), 0);
            if r < 0 {
                // Source does not support AT_EOF: fall back to the internal flag
                // (set when the last READ reached end of stream), like libzip's
                // read-ahead EOF tracking for non-supporting sources.
                Ok(s.eof.load(Ordering::Relaxed))
            } else {
                Ok(r.min(1) as c_int)
            }
        },
        -1,
    )
}

/// `zip_source_is_seekable(src)` — return 1 if the source supports seeking, 0
/// otherwise.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_is_seekable(src: H) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let supported = s.query_supports();
            Ok(if supported & src_bit(ZIP_SOURCE_SEEK) != 0 {
                1
            } else {
                0
            })
        },
        -1,
    )
}

/// `zip_source_keep(src)` — increment the reference count.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_keep(src: H) {
    if let Some(s) = src.cast::<ZipSource>().as_ref() {
        s.refcount.fetch_add(1, Ordering::Relaxed);
    }
}

/// `zip_source_error(src)` — return a pointer to the source's error object.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_error(src: H) -> *mut zip_error {
    let s = src.cast::<ZipSource>().as_ref();
    match s {
        Some(s) => {
            let mut g = s.error.lock().unwrap_or_else(|e| e.into_inner());
            // Refresh the cached C string pointer.
            let code = g.ze.zip_err;
            let owned = CString::new(err_str(code)).unwrap_or_default();
            g.ze.str = owned.as_ptr() as *mut c_char;
            g.owned = Some(owned);
            &mut g.ze as *mut zip_error
        }
        None => std::ptr::null_mut(),
    }
}

/// `zip_source_is_deleted(src)` — return 1 if the underlying file was deleted,
/// else 0.
///
/// # Safety
///
/// `src` a valid `zip_source_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_is_deleted(src: H) -> c_int {
    guarded(
        || {
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            // Memory backends are never deleted; function backends advertise
            // REMOVE support if they can delete.
            let supported = s.query_supports();
            Ok(if supported & src_bit(ZIP_SOURCE_REMOVE) != 0 {
                1
            } else {
                0
            })
        },
        -1,
    )
}

/// `zip_source_get_file_attributes(src, attrs)` — fill `attrs` (all-zero valid).
///
/// # Safety
///
/// `src` a valid `zip_source_t*`; `attrs` a valid `zip_file_attributes_t*`.
#[no_mangle]
pub unsafe extern "C" fn zip_source_get_file_attributes(
    src: H,
    attrs: *mut zip_file_attributes,
) -> c_int {
    guarded(
        || {
            if src.is_null() || attrs.is_null() {
                return Err(-1);
            }
            let s = src.cast::<ZipSource>().as_ref().ok_or(-1)?;
            let r = s.dispatch(
                ZIP_SOURCE_GET_FILE_ATTRIBUTES,
                attrs as *mut c_void,
                std::mem::size_of::<zip_file_attributes>() as u64,
            );
            if r < 0 {
                // Not supported: leave `attrs` untouched (zeroed by caller).
                Err(-1)
            } else {
                Ok(0)
            }
        },
        -1,
    )
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
            let data = src.read_all().map_err(|_| {
                z.set_err(ZipErrorCode::Read.as_i32(), 0);
                -1
            })?;
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
            let data = src.read_all().map_err(|_| {
                z.set_err(ZipErrorCode::Read.as_i32(), 0);
                -1
            })?;
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
            let g = z.archive_comment.lock().unwrap_or_else(|e| e.into_inner());
            let p = g.as_ref().map(|c| c.as_ptr() as *const c_char).unwrap_or(std::ptr::null());
            if !lenp.is_null() {
                unsafe {
                    *lenp = g
                        .as_ref()
                        .map(|c| c.len().saturating_sub(1) as c_int) // strip the stored trailing NUL
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
/// Legacy libzip alias for [`zip_file_get_comment`] (identical signature and
/// behavior). Exposed so both spellings resolve in the C ABI.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `lenp` (if non-null) writable storage.
#[no_mangle]
pub unsafe extern "C" fn zip_get_file_comment(
    zh: H,
    index: u64,
    lenp: *mut c_int,
    flags: u32,
) -> *const c_char {
    zip_file_get_comment(zh, index, lenp, flags)
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
            let g = z.comments.lock().unwrap_or_else(|e| e.into_inner());
            let p = g
                .get(index as usize)
                .and_then(|c| c.as_ref())
                .map(|c| c.as_ptr())
                .unwrap_or(std::ptr::null());
            if !lenp.is_null() {
                unsafe {
                    *lenp = g
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
            z.ensure_extra_slot(index);
            let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
            let d = v.get(index as usize).ok_or(-1)?;
            Ok(d.len() as i16)
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
            z.ensure_extra_slot(index);
            let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
            let d = v.get(index as usize).ok_or(-1)?;
            Ok(d.iter().filter(|(i, _)| *i == id).count() as i16)
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
            z.ensure_extra_slot(index);
            let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
            let d = v.get(index as usize).ok_or(-1)?;
            let mut n = 0u16;
            let want = if idxp.is_null() { 0 } else { unsafe { *idxp } };
            let mut found: Option<&Vec<u8>> = None;
            for (i, data) in d {
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
            z.ensure_extra_slot(index);
            let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
            let d = v.get(index as usize).ok_or(-1)?;
            let mut n = 0u16;
            let mut found: Option<&Vec<u8>> = None;
            for (i, data) in d {
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
// Write-path metadata (Phase 3): comments, extra fields, mtime/dostime,
// external attributes, per-entry compression
// ---------------------------------------------------------------------------

/// Set the archive (EOCD) comment to the `len` bytes at `comment` (or remove
/// it when `comment` is NULL / `len` is 0). Applied on [`zip_close`].
///
/// # Safety
///
/// `zh` must be a valid, open handle; `comment` (if non-null) must point to at
/// least `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_set_archive_comment(
    zh: H,
    comment: *const c_char,
    len: u16,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if comment.is_null() && len > 0 {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let bytes = if len == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(comment.cast::<u8>(), len as usize).to_vec() }
            };
            {
                let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.archive_comment = Some(bytes.clone());
            }
            let mut ov = z.archive_comment.lock().unwrap_or_else(|e| e.into_inner());
            *ov = if bytes.is_empty() {
                None
            } else {
                // Store raw comment bytes + a trailing NUL so the getter can
                // return a stable C-string pointer while still reporting the raw
                // length (which may include a NUL the C caller passed as part of
                // the length, matching libzip).
                let mut buf = bytes.clone();
                buf.push(0);
                Some(buf)
            };
            Ok(0)
        },
        -1,
    )
}

/// Set the comment of the entry at `index` to the `len` bytes at `comment`
/// (or remove it when `comment` is NULL / `len` is 0). Applied on
/// [`zip_close`].
///
/// # Safety
///
/// `zh` must be a valid, open handle; `comment` (if non-null) must point to at
/// least `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_file_set_comment(
    zh: H,
    index: u64,
    comment: *const c_char,
    len: u16,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            if comment.is_null() && len > 0 {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let bytes = if len == 0 {
                None
            } else {
                Some(
                    unsafe { std::slice::from_raw_parts(comment.cast::<u8>(), len as usize) }
                        .to_vec(),
                )
            };
            z.set_file_comment_pending(index, bytes)?;
            Ok(0)
        },
        -1,
    )
}

/// Deprecated alias for [`zip_file_set_comment`]: `len` is an `int` length, or
/// -1 to use `strlen(comment)`.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `comment` a NUL-terminated C string when
/// `len < 0`, else a pointer to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_set_file_comment(
    zh: H,
    index: u64,
    comment: *const c_char,
    len: c_int,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let bytes = if comment.is_null() {
                None
            } else if len < 0 {
                Some(CStr::from_ptr(comment).to_bytes().to_vec())
            } else {
                Some(unsafe { std::slice::from_raw_parts(comment.cast::<u8>(), len as usize) }
                    .to_vec())
            };
            z.set_file_comment_pending(index, bytes)?;
            Ok(0)
        },
        -1,
    )
}

/// Add or replace an extra field of the entry at `index`.
///
/// `id`/`ef_idx` identify the field: `ZIP_EXTRA_FIELD_NEW` (0xFFFF) appends a
/// new field; otherwise the `ef_idx`-th existing field with that `id` is
/// replaced (an index equal to the current count appends). `flags` selects
/// placement (local/central); if neither is given the field is stored in both.
/// Applied on [`zip_close`]. Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `data` (if non-null) must point to `len`
/// readable bytes.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_field_set(
    zh: H,
    index: u64,
    id: u16,
    ef_idx: u16,
    data: *const u8,
    len: u16,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            // Internal fields (ZIP64, WinZip AES) cannot be set by users.
            if id == ZIP_EF_WINZIP_AES || id == ZIP_EF_ZIP64 {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            if data.is_null() && len > 0 {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let bytes = if len == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(data, len as usize).to_vec() }
            };
            // Validate ef_idx against the count of existing fields with this id.
            if ef_idx != ZIP_EXTRA_FIELD_NEW {
                let count = {
                    let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
                    v.get(index as usize)
                        .map(|f| f.iter().filter(|(i, _)| *i == id).count())
                        .unwrap_or(0)
                };
                if (ef_idx as usize) > count {
                    z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                    return Err(-1);
                }
            }
            {
                let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending
                    .extra_field_ops
                    .push((index, ExtraFieldOp::Set { id, ef_idx, data: bytes }));
            }
            {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                z.refresh_extra_fields(&pending, index);
            }
            Ok(0)
        },
        -1,
    )
}

/// Delete the `ef_idx`-th extra field of the entry at `index` (or all with
/// `ZIP_EXTRA_FIELD_ALL`). Applied on [`zip_close`]. Returns 0 on success,
/// -1 (with `ZIP_ER_NOENT`) if the index is out of range.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_field_delete(
    zh: H,
    index: u64,
    ef_idx: u16,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let count = {
                let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
                v.get(index as usize).map(|f| f.len()).unwrap_or(0)
            };
            if ef_idx != ZIP_EXTRA_FIELD_ALL && (ef_idx as usize) >= count {
                z.set_err(ZipErrorCode::Noent.as_i32(), 0);
                return Err(-1);
            }
            {
                let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.extra_field_ops.push((index, ExtraFieldOp::Delete { ef_idx }));
            }
            {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                z.refresh_extra_fields(&pending, index);
            }
            Ok(0)
        },
        -1,
    )
}

/// Delete the `ef_idx`-th extra field with id `id` of the entry at `index`
/// (or all with that id via `ZIP_EXTRA_FIELD_ALL`). Applied on [`zip_close`].
/// Returns 0 on success, -1 (with `ZIP_ER_NOENT`) if no such field exists.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_extra_field_delete_by_id(
    zh: H,
    index: u64,
    id: u16,
    ef_idx: u16,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let count = {
                let v = z.extra_fields.lock().unwrap_or_else(|e| e.into_inner());
                v.get(index as usize)
                    .map(|f| f.iter().filter(|(i, _)| *i == id).count())
                    .unwrap_or(0)
            };
            if id != ZIP_EXTRA_FIELD_ALL
                && ef_idx != ZIP_EXTRA_FIELD_ALL
                && (ef_idx as usize) >= count
            {
                z.set_err(ZipErrorCode::Noent.as_i32(), 0);
                return Err(-1);
            }
            {
                let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.extra_field_ops.push((index, ExtraFieldOp::DeleteById { id, ef_idx }));
            }
            {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                z.refresh_extra_fields(&pending, index);
            }
            Ok(0)
        },
        -1,
    )
}

/// Set the last-modification time of the entry at `index` from a Unix
/// timestamp (converted to DOS via the local timezone on close). Applied on
/// [`zip_close`]. Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_set_mtime(
    zh: H,
    index: u64,
    mtime: i64,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.mtimes.retain(|(i, _)| *i != index);
            pending.mtimes.push((index, mtime));
            Ok(0)
        },
        -1,
    )
}

/// Set the last-modification time of the entry at `index` from a DOS
/// `(dtime, ddate)` pair. Applied on [`zip_close`]. Returns 0 on success, -1
/// on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_set_dostime(
    zh: H,
    index: u64,
    dtime: u16,
    ddate: u16,
    _flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.dostimes.retain(|(i, _, _)| *i != index);
            pending.dostimes.push((index, dtime, ddate));
            Ok(0)
        },
        -1,
    )
}

/// Set the external (host-specific) attributes of the entry at `index`.
/// `opsys` is the host system (`ZIP_OPSYS_UNIX` etc.); `attributes` is the
/// host-specific attribute bitmask. Applied on [`zip_close`]. Returns 0 on
/// success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_file_set_external_attributes(
    zh: H,
    index: u64,
    _flags: u32,
    opsys: u8,
    attributes: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.ext_attrs.retain(|(i, _, _)| *i != index);
            pending.ext_attrs.push((index, opsys, attributes));
            Ok(0)
        },
        -1,
    )
}

/// Return the external (host-specific) attributes of the entry at `index`,
/// writing the host system to `opsys` and the attribute bitmask to
/// `attributes` (if non-null). Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle; `opsys`/`attributes` (if non-null) writable.
#[no_mangle]
pub unsafe extern "C" fn zip_file_get_external_attributes(
    zh: H,
    index: u64,
    _flags: u32,
    opsys: *mut u8,
    attributes: *mut u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let efa = {
                let pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
                z.effective_ext_attrs(&pending, index)
            };
            match efa {
                Some((o, a)) => {
                    if !opsys.is_null() {
                        unsafe { *opsys = o };
                    }
                    if !attributes.is_null() {
                        unsafe { *attributes = a };
                    }
                    Ok(0)
                }
                None => Err(-1),
            }
        },
        -1,
    )
}

/// Zero-initialize a `zip_file_attributes_t` (version 1, no valid fields).
///
/// # Safety
///
/// `attrs` must point to writable `zip_file_attributes` storage.
#[no_mangle]
pub unsafe extern "C" fn zip_file_attributes_init(attrs: *mut zip_file_attributes) {
    if !attrs.is_null() {
        unsafe {
            (*attrs).valid = 0;
            (*attrs).version = 1;
            (*attrs).host_system = ZIP_OPSYS_UNIX;
            (*attrs).ascii = 0;
            (*attrs).version_needed = 20;
            (*attrs).external_file_attributes = 0;
            (*attrs).general_purpose_bit_flags = 0;
            (*attrs).general_purpose_bit_mask = 0;
        }
    }
}

/// Set the compression method (and level) for the entry at `index`, applied on
/// [`zip_close`]. `ZIP_CM_DEFAULT` (-1) resets to the archive default,
/// `ZIP_CM_STORE` (0) stores uncompressed, `ZIP_CM_DEFLATE` (8) deflates with
/// `flags` as the level. An unsupported method returns `ZIP_ER_COMPNOTSUPP`.
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `zh` must be a valid, open handle.
#[no_mangle]
pub unsafe extern "C" fn zip_set_file_compression(
    zh: H,
    index: u64,
    method: i32,
    flags: u32,
) -> c_int {
    guarded(
        || {
            let z = zh.cast::<Zip>().as_ref().ok_or(-1)?;
            z.check_writable()?;
            if !z.valid_index(index) {
                z.set_err(ZipErrorCode::Inval.as_i32(), 0);
                return Err(-1);
            }
            let supported = method == ZIP_CM_DEFAULT || method == ZIP_CM_STORE || method == ZIP_CM_DEFLATE;
            if !supported {
                z.set_err(ZipErrorCode::Compnotsupp.as_i32(), 0);
                return Err(-1);
            }
            let mut pending = z.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.compressions.retain(|(i, _, _)| *i != index);
            pending.compressions.push((index, method, flags));
            Ok(0)
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
            // Phase 3 write-path metadata symbols.
            "zip_file_set_comment",
            "zip_set_file_comment",
            "zip_set_archive_comment",
            "zip_get_file_comment",
            "zip_file_extra_field_set",
            "zip_file_extra_field_delete",
            "zip_file_extra_field_delete_by_id",
            "zip_file_set_mtime",
            "zip_file_set_dostime",
            "zip_file_set_external_attributes",
            "zip_file_get_external_attributes",
            "zip_file_attributes_init",
            "zip_set_file_compression",
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

    // ---- Phase 3: write-path metadata round-trips ----

    const ZIP_FL_ENC_UTF_8: u32 = 2048;

    /// Create a new archive with a single entry `a.txt`, returning its index.
    unsafe fn open_single_file_archive(cpath: *const c_char) -> (H, i64) {
        // Open with ZIP_TRUNCATE so the archive always starts fresh, even when
        // the temp path already exists from a prior call within the same test
        // (some round-trip tests open the same path twice).
        let zh = unsafe { zip_open(cpath, ZIP_CREATE | ZIP_TRUNCATE, std::ptr::null_mut()) };
        assert!(!zh.is_null(), "zip_open ZIP_CREATE failed");
        let data = b"phase3 metadata payload content".to_vec();
        let src =
            unsafe { zip_source_buffer(zh, data.as_ptr() as *const c_void, data.len() as u64, 0) };
        assert!(!src.is_null());
        let idx = unsafe { zip_file_add(zh, CString::new("a.txt").unwrap().as_ptr(), src, 0) };
        assert!(idx >= 0, "zip_file_add failed");
        unsafe { zip_source_free(src) };
        (zh, idx)
    }

    /// TC-1: archive + file comments round-trip byte-exact (incl. non-ASCII).
    #[test]
    fn comments_round_trip() {
        let path = temp_path("cmts");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let (zh, idx) = unsafe { open_single_file_archive(cpath.as_ptr()) };

        let arch_cmt = "archive comment"; // 16 bytes
        let file_cmt = "file comment"; // 12 bytes
        assert_eq!(
            unsafe {
                zip_set_archive_comment(zh, CString::new(arch_cmt).unwrap().as_ptr(), 16)
            },
            0
        );
        assert_eq!(
            unsafe {
                zip_file_set_comment(zh, idx as u64, CString::new(file_cmt).unwrap().as_ptr(), 12, 0)
            },
            0
        );
        // Non-ASCII (UTF-8) comment bytes survive verbatim.
        let uni = "ünïcode-çomment"; // bytes > ASCII
        assert_eq!(
            unsafe {
                zip_file_set_comment(
                    zh,
                    idx as u64,
                    CString::new(uni).unwrap().as_ptr(),
                    uni.len() as u16,
                    0,
                )
            },
            0
        );
        assert_eq!(unsafe { zip_close(zh) }, 0);

        // Reopen and verify all three.
        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut alen: c_int = 0;
        let ac = unsafe { zip_get_archive_comment(zh2, &mut alen, 0) };
        assert_eq!(cstr(ac), arch_cmt);
        assert_eq!(alen, 16);
        let mut flen: c_int = 0;
        let fc = unsafe { zip_file_get_comment(zh2, idx as u64, &mut flen, 0) };
        assert_eq!(cstr(fc), uni);
        assert_eq!(flen, uni.len() as c_int);

        // Setting an empty/NULL comment removes it.
        assert_eq!(unsafe { zip_file_set_comment(zh2, idx as u64, std::ptr::null(), 0, 0) }, 0);
        assert_eq!(unsafe { zip_set_archive_comment(zh2, std::ptr::null(), 0) }, 0);
        assert_eq!(unsafe { zip_close(zh2) }, 0);

        let zh3 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh3.is_null());
        assert!(unsafe { zip_get_archive_comment(zh3, std::ptr::null_mut(), 0) }.is_null());
        assert!(unsafe { zip_file_get_comment(zh3, idx as u64, std::ptr::null_mut(), 0) }.is_null());
        unsafe { zip_close(zh3) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-2: extra fields round-trip byte-exact (incl. binary data) + count.
    #[test]
    fn extra_field_round_trip() {
        let path = temp_path("ef");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let (zh, idx) = unsafe { open_single_file_archive(cpath.as_ptr()) };

        let data: [u8; 5] = [0xDE, 0xAD, 0xBE, 0xEF, 0x00]; // binary incl. NUL
        assert_eq!(
            unsafe {
                zip_file_extra_field_set(
                    zh,
                    idx as u64,
                    0xCAFE,
                    0,
                    data.as_ptr(),
                    5,
                    ZIP_FL_ENC_UTF_8,
                )
            },
            0
        );
        // Count reflects the added field immediately.
        assert_eq!(unsafe { zip_file_extra_fields_count(zh, idx as u64, 0) }, 1);
        assert_eq!(
            unsafe { zip_file_extra_fields_count_by_id(zh, idx as u64, 0xCAFE, 0) },
            1
        );
        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        assert_eq!(unsafe { zip_file_extra_fields_count(zh2, idx as u64, 0) }, 1);
        let mut elen: u16 = 0;
        let p = unsafe { zip_file_extra_field_get_by_id(zh2, idx as u64, 0xCAFE, 0, &mut elen, 0) };
        assert!(!p.is_null(), "extra field must be present after reopen");
        assert_eq!(elen, 5);
        let got = unsafe { std::slice::from_raw_parts(p, elen as usize) };
        assert_eq!(got, &data);
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-3: mtime and dos time round-trip (timezone-aware) via zip_stat.
    #[test]
    fn mtime_round_trip() {
        let path = temp_path("mtime");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let (zh, idx) = unsafe { open_single_file_archive(cpath.as_ptr()) };

        // mtime: pick a timestamp aligned to an even local second for an exact
        // round-trip.
        let ut: i64 = 1600000000; // 2020-09-13 local
        assert_eq!(unsafe { zip_file_set_mtime(zh, idx as u64, ut, 0) }, 0);
        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, idx as u64, 0, &mut sb) }, 0);
        assert_eq!(sb.mtime, ut, "mtime must round-trip exactly");
        unsafe { zip_close(zh2) };

        // dos time: set a DOS timestamp and verify it reads back correctly.
        let (zh3, idx3) = unsafe { open_single_file_archive(cpath.as_ptr()) };
        let dtime: u16 = (8 << 11) | (26 << 5) | (40 / 2); // 08:26:40
        let ddate: u16 = (40 << 9) | (9 << 5) | 13; // 2020-09-13
        assert_eq!(unsafe { zip_file_set_dostime(zh3, idx3 as u64, dtime, ddate, 0) }, 0);
        assert_eq!(unsafe { zip_close(zh3) }, 0);

        let zh4 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh4.is_null());
        let mut sb2 = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh4, idx3 as u64, 0, &mut sb2) }, 0);
        assert_eq!(
            sb2.mtime,
            zip_core::archive::dos_to_unix(dtime, ddate) as i64
        );
        unsafe { zip_close(zh4) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4: external attributes round-trip + zip_file_attributes_init.
    #[test]
    fn external_attributes_round_trip() {
        let path = temp_path("extattr");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let (zh, idx) = unsafe { open_single_file_archive(cpath.as_ptr()) };

        let attrs: u32 = (0o100644u32) << 16; // regular file, rw-r--r--
        assert_eq!(
            unsafe { zip_file_set_external_attributes(zh, idx as u64, 0, ZIP_OPSYS_UNIX, attrs) },
            0
        );
        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut opsys: u8 = 0;
        let mut attr: u32 = 0;
        assert_eq!(
            unsafe { zip_file_get_external_attributes(zh2, idx as u64, 0, &mut opsys, &mut attr) },
            0
        );
        assert_eq!(opsys, ZIP_OPSYS_UNIX);
        assert_eq!(attr, attrs);
        unsafe { zip_close(zh2) };

        // zip_file_attributes_init zero-initializes with version 1.
        let mut a: zip_file_attributes = unsafe { std::mem::zeroed() };
        unsafe { zip_file_attributes_init(&mut a) };
        assert_eq!(a.valid, 0);
        assert_eq!(a.version, 1);
        assert_eq!(a.host_system, ZIP_OPSYS_UNIX);
        assert_eq!(a.external_file_attributes, 0);

        std::fs::remove_file(&path).ok();
    }

    /// TC-6: zip_set_file_compression selects Store/Deflate per entry.
    #[test]
    fn compression_method_selection() {
        let path = temp_path("comp");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let zh = unsafe { zip_open(cpath.as_ptr(), ZIP_CREATE, std::ptr::null_mut()) };
        assert!(!zh.is_null());
        // Two files with compressible content.
        let d0 = b"compress me compress me compress me ".repeat(40);
        let d1 = b"also compressible payload content ".repeat(40);
        let s0 = unsafe {
            zip_source_buffer(zh, d0.as_ptr() as *const c_void, d0.len() as u64, 0)
        };
        let s1 = unsafe {
            zip_source_buffer(zh, d1.as_ptr() as *const c_void, d1.len() as u64, 0)
        };
        assert_eq!(
            unsafe { zip_file_add(zh, CString::new("store.txt").unwrap().as_ptr(), s0, 0) },
            0
        );
        assert_eq!(
            unsafe { zip_file_add(zh, CString::new("deflate.txt").unwrap().as_ptr(), s1, 0) },
            1
        );
        unsafe { zip_source_free(s0) };
        unsafe { zip_source_free(s1) };

        // Per-entry Store (0) and Deflate level 9 (8).
        assert_eq!(unsafe { zip_set_file_compression(zh, 0, ZIP_CM_STORE, 0) }, 0);
        assert_eq!(unsafe { zip_set_file_compression(zh, 1, ZIP_CM_DEFLATE, 9) }, 0);
        // Unsupported method -> ZIP_ER_COMPNOTSUPP (16).
        assert_eq!(unsafe { zip_set_file_compression(zh, 0, 99, 0) }, -1);
        assert_eq!(
            cstr(unsafe { zip_strerror(zh) }),
            "Compression method not supported"
        );
        assert_eq!(unsafe { zip_close(zh) }, 0);

        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        let mut sb0 = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, 0, 0, &mut sb0) }, 0);
        assert_eq!(sb0.comp_method, ZIP_CM_STORE as u16);
        assert_eq!(sb0.size, sb0.comp_size, "stored entry is uncompressed");

        let mut sb1 = zero_stat();
        assert_eq!(unsafe { zip_stat_index(zh2, 1, 0, &mut sb1) }, 0);
        assert_eq!(sb1.comp_method, ZIP_CM_DEFLATE as u16);
        assert!(sb1.comp_size < sb1.size, "deflated entry must shrink");

        // Read both back byte-identically.
        let fh0 = unsafe { zip_fopen_index(zh2, 0, 0) };
        assert!(!fh0.is_null());
        assert_eq!(unsafe { read_ffi_full(fh0) }, d0);
        unsafe { zip_fclose(fh0) };
        let fh1 = unsafe { zip_fopen_index(zh2, 1, 0) };
        assert!(!fh1.is_null());
        assert_eq!(unsafe { read_ffi_full(fh1) }, d1);
        unsafe { zip_fclose(fh1) };

        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-7: extra-field set/delete round-trip; counts reflect changes.
    #[test]
    fn extra_field_delete_round_trip() {
        let path = temp_path("efdel");
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let (zh, idx) = unsafe { open_single_file_archive(cpath.as_ptr()) };

        let a: [u8; 2] = [0x11, 0x22];
        let b: [u8; 3] = [0x33, 0x44, 0x55];
        assert_eq!(
            unsafe { zip_file_extra_field_set(zh, idx as u64, 0xCAFE, 0, a.as_ptr(), 2, 0) },
            0
        );
        assert_eq!(
            unsafe { zip_file_extra_field_set(zh, idx as u64, 0xBEEF, 0, b.as_ptr(), 3, 0) },
            0
        );
        assert_eq!(unsafe { zip_file_extra_fields_count(zh, idx as u64, 0) }, 2);

        // Delete by index 0 (removes the 0xCAFE field).
        assert_eq!(unsafe { zip_file_extra_field_delete(zh, idx as u64, 0, 0) }, 0);
        assert_eq!(unsafe { zip_file_extra_fields_count(zh, idx as u64, 0) }, 1);

        // Deleting a nonexistent id returns an error, no panic.
        assert_eq!(
            unsafe { zip_file_extra_field_delete_by_id(zh, idx as u64, 0x9999, 0, 0) },
            -1
        );
        assert_ne!(cstr(unsafe { zip_strerror(zh) }), "No error");

        // Delete the remaining field by id.
        assert_eq!(
            unsafe { zip_file_extra_field_delete_by_id(zh, idx as u64, 0xBEEF, 0, 0) },
            0
        );
        assert_eq!(unsafe { zip_file_extra_fields_count(zh, idx as u64, 0) }, 0);

        assert_eq!(unsafe { zip_close(zh) }, 0);

        // Deletions persist after write/close/reopen.
        let zh2 = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!zh2.is_null());
        assert_eq!(unsafe { zip_file_extra_fields_count(zh2, idx as u64, 0) }, 0);
        unsafe { zip_close(zh2) };
        std::fs::remove_file(&path).ok();
    }

    // ---------------------------------------------------------------------
    // Phase 4a: streaming zip_source_* core
    // ---------------------------------------------------------------------

    /// Read all bytes from an open source handle via `zip_source_read`.
    unsafe fn read_source_all(src: H) -> Vec<u8> {
        assert_eq!(unsafe { zip_source_open(src) }, 0, "zip_source_open");
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { zip_source_read(src, buf.as_mut_ptr() as *mut c_void, 8192) };
            assert!(n >= 0, "zip_source_read error");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { zip_source_close(src) };
        out
    }

    /// TC-4a-1: `zip_source_file_create` opens + reads a real archive
    /// byte-identically; stat/is_seekable/at_eof behave like C.
    #[test]
    fn source_file_read() {
        let path = temp_path("src_file");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let ground_truth = std::fs::read(&path).unwrap();

        let mut errp: c_int = -1;
        let src = unsafe {
            zip_source_file_create(cpath.as_ptr(), 0, -1, &mut errp)
        };
        assert!(!src.is_null(), "zip_source_file_create errp={errp}");

        // Seekable, not at EOF before read.
        assert_eq!(unsafe { zip_source_is_seekable(src) }, 1);
        assert_eq!(unsafe { zip_source_at_eof(src) }, 0);

        // stat returns the file size.
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_source_stat(src, &mut sb) }, 0);
        assert_eq!(sb.size, ground_truth.len() as u64);
        assert_ne!(sb.valid & ZIP_STAT_SIZE, 0);
        assert_ne!(sb.valid & ZIP_STAT_MTIME, 0);

        // Reading yields the file bytes byte-identically.
        let bytes = unsafe { read_source_all(src) };
        assert_eq!(bytes, ground_truth);
        assert_eq!(unsafe { zip_source_at_eof(src) }, 1, "EOF after exhausting");

        unsafe { zip_source_free(src) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4a-1b: `zip_source_file_create` with a start/len window.
    #[test]
    fn source_file_window_range() {
        let path = temp_path("src_file_win");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let ground_truth = std::fs::read(&path).unwrap();

        let mut errp: c_int = -1;
        let src =
            unsafe { zip_source_file_create(cpath.as_ptr(), 10, 20, &mut errp) };
        assert!(!src.is_null());
        let bytes = unsafe { read_source_all(src) };
        assert_eq!(bytes, ground_truth[10..30].to_vec());
        unsafe { zip_source_free(src) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4a-2: `zip_source_function_create` callbacks invoked with correct
    /// command semantics (OPEN before READ, STAT size, SEEK/TELL, SUPPORTS,
    /// CLOSE exactly once, ERROR reachable).
    ///
    /// The callback implements a read-only source over a fixed byte slice.
    #[test]
    fn source_function_callbacks() {
        static DATA: &[u8] = b"function-source payload 0123456789";
        use std::sync::atomic::{AtomicI32, AtomicUsize};

        struct FnState {
            open_count: AtomicI32,
            close_count: AtomicI32,
            pos: AtomicUsize,
            fail_next_read: AtomicI32,
        }

        unsafe extern "C" fn cb(
            ud: *mut c_void,
            data: *mut c_void,
            len: u64,
            cmd: c_int,
        ) -> i64 {
            let st = unsafe { &mut *(ud.cast::<FnState>()) };
            match cmd {
                ZIP_SOURCE_OPEN => {
                    st.open_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    st.pos.store(0, std::sync::atomic::Ordering::SeqCst);
                    0
                }
                ZIP_SOURCE_CLOSE => {
                    st.close_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    0
                }
                ZIP_SOURCE_READ => {
                    if st.fail_next_read.swap(0, std::sync::atomic::Ordering::SeqCst) != 0 {
                        return -1;
                    }
                    let p = st.pos.load(std::sync::atomic::Ordering::SeqCst);
                    let remaining = DATA.len().saturating_sub(p);
                    let n = remaining.min(len as usize);
                    if n > 0 {
                        let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), n) };
                        out.copy_from_slice(&DATA[p..p + n]);
                        st.pos.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
                    }
                    n as i64
                }
                ZIP_SOURCE_STAT => {
                    let sb = unsafe { &mut *(data.cast::<zip_stat>()) };
                    sb.valid = ZIP_STAT_SIZE;
                    sb.size = DATA.len() as u64;
                    0
                }
                ZIP_SOURCE_SEEK => {
                    let args = unsafe { &*(data.cast::<ZipSourceArgsSeek>()) };
                    let size = DATA.len() as i64;
                    let cur = st.pos.load(std::sync::atomic::Ordering::SeqCst) as i64;
                    let target = match args.whence {
                        SEEK_SET => args.offset,
                        SEEK_CUR => cur + args.offset,
                        SEEK_END => size + args.offset,
                        _ => return -1,
                    };
                    if target < 0 {
                        return -1;
                    }
                    st.pos.store(target as usize, std::sync::atomic::Ordering::SeqCst);
                    0
                }
                ZIP_SOURCE_TELL => {
                    st.pos.load(std::sync::atomic::Ordering::SeqCst) as i64
                }
                ZIP_SOURCE_SUPPORTS => SRC_SEEKABLE,
                ZIP_SOURCE_ERROR => 0,
                ZIP_SOURCE_FREE => 0,
                _ => -1,
            }
        }

        let mut state = FnState {
            open_count: AtomicI32::new(0),
            close_count: AtomicI32::new(0),
            pos: AtomicUsize::new(0),
            fail_next_read: AtomicI32::new(0),
        };
        let mut errp: c_int = -1;
        let src = unsafe {
            zip_source_function_create(
                Some(cb),
                (&mut state as *mut FnState) as *mut c_void,
                &mut errp,
            )
        };
        assert!(!src.is_null(), "zip_source_function_create errp={errp}");

        assert_eq!(unsafe { zip_source_is_seekable(src) }, 1, "SUPPORTS seekable");
        let bytes = unsafe { read_source_all(src) };
        assert_eq!(bytes, DATA.to_vec(), "byte-identical to the slice");
        assert_eq!(
            state.open_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "OPEN exactly once"
        );
        assert_eq!(
            state.close_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "CLOSE exactly once"
        );

        // SEEK/TELL: seek within the source and read the sub-range.
        assert_eq!(unsafe { zip_source_open(src) }, 0);
        assert_eq!(unsafe { zip_source_seek(src, 10, SEEK_SET) }, 0);
        assert_eq!(unsafe { zip_source_tell(src) }, 10);
        let mut b = [0u8; 5];
        assert_eq!(unsafe { zip_source_read(src, b.as_mut_ptr() as *mut c_void, 5) }, 5);
        assert_eq!(&b, &DATA[10..15]);
        unsafe { zip_source_close(src) };

        // ERROR is reachable: a failing READ returns -1 without panicking.
        state.fail_next_read.store(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(unsafe { zip_source_open(src) }, 0);
        assert_eq!(unsafe { zip_source_read(src, b.as_mut_ptr() as *mut c_void, 5) }, -1);
        unsafe { zip_source_close(src) };

        unsafe { zip_source_free(src) };
    }

    /// TC-4a-3: `zip_source_window_create` exposes exactly `len` bytes at
    /// `offset`; EOF after the last byte; seeking within the window is
    /// relative to the window.
    #[test]
    fn source_window_read() {
        let path = temp_path("src_win");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let full = std::fs::read(&path).unwrap();

        let mut errp: c_int = -1;
        let base =
            unsafe { zip_source_file_create(cpath.as_ptr(), 0, -1, &mut errp) };
        assert!(!base.is_null());
        let win = unsafe { zip_source_window_create(base, 5, 100, &mut errp) };
        assert!(!win.is_null(), "zip_source_window_create errp={errp}");

        assert_eq!(unsafe { zip_source_is_seekable(win) }, 1);
        assert_eq!(unsafe { zip_source_at_eof(win) }, 0);
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_source_stat(win, &mut sb) }, 0);
        assert_eq!(sb.size, 100, "window size = len");

        let bytes = unsafe { read_source_all(win) };
        assert_eq!(bytes, full[5..105].to_vec(), "exact window sub-range");
        assert_eq!(unsafe { zip_source_at_eof(win) }, 1, "EOF after last byte");

        // Seeking is relative to the window.
        assert_eq!(unsafe { zip_source_open(win) }, 0);
        assert_eq!(unsafe { zip_source_seek(win, 50, SEEK_SET) }, 0);
        let mut b = [0u8; 10];
        assert_eq!(unsafe { zip_source_read(win, b.as_mut_ptr() as *mut c_void, 10) }, 10);
        assert_eq!(&b, &full[5 + 50..5 + 60]);
        unsafe { zip_source_close(win) };
        unsafe { zip_source_free(win) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4a-4: `zip_source_zip_create` reads an entry from another archive as
    /// a source; adding it into a second archive copies the bytes correctly.
    #[test]
    fn source_zip_entry() {
        let path = temp_path("src_zip");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        // Source archive containing the entry.
        let srcza = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!srcza.is_null());

        // Build an in-memory archive to receive the copied entry.
        let dst = unsafe { zip_open(std::ptr::null_mut(), ZIP_CREATE | ZIP_TRUNCATE, std::ptr::null_mut()) };
        // The dest archive must have a path to write on close; use a temp file.
        let dst_path = temp_path("src_zip_dst");
        let dst_cpath = CString::new(dst_path.to_string_lossy().as_bytes()).unwrap();
        unsafe { zip_close(dst) };
        let dst = unsafe { zip_open(dst_cpath.as_ptr(), ZIP_CREATE | ZIP_TRUNCATE, std::ptr::null_mut()) };
        assert!(!dst.is_null());

        let mut errp: c_int = -1;
        let src =
            unsafe { zip_source_zip_create(srcza, 0, 0, 0, -1, &mut errp) };
        assert!(!src.is_null(), "zip_source_zip_create errp={errp}");

        // Verify the source reads the entry's (decompressed) bytes exactly.
        let expected = b"hello ffi read path content "
            .repeat(20)
            .to_vec();
        let got = unsafe { read_source_all(src) };
        assert_eq!(got, expected, "entry content byte-identical");
        unsafe { zip_source_free(src) };

        // Copy it into the destination archive via zip_file_add.
        let src2 = unsafe { zip_source_zip_create(srcza, 0, 0, 0, -1, &mut errp) };
        assert!(!src2.is_null());
        let name = CString::new("copied.txt").unwrap();
        let idx = unsafe { zip_file_add(dst, name.as_ptr(), src2, 0) };
        assert!(idx >= 0, "zip_file_add of zip_source failed");
        unsafe { zip_source_free(src2) };
        assert_eq!(unsafe { zip_close(dst) }, 0);

        // Reopen dest and verify the copied entry bytes.
        let dst2 = unsafe { zip_open(dst_cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!dst2.is_null());
        let fh = unsafe { zip_fopen_index(dst2, idx as u64, 0) };
        assert!(!fh.is_null());
        let mut copied = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = unsafe { zip_fread(fh, buf.as_mut_ptr() as *mut c_void, 512) };
            if n <= 0 {
                break;
            }
            copied.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { zip_fclose(fh) };
        assert_eq!(copied, expected, "copied entry matches original");
        unsafe { zip_close(dst2) };
        unsafe { zip_close(srcza) };
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&dst_path).ok();
    }

    /// TC-4a-4b: `zip_source_zip_create` with `ZIP_FL_COMPRESSED` exposes the
    /// raw stored bytes.
    #[test]
    fn source_zip_compressed() {
        let path = temp_path("src_zip_c");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let za = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!za.is_null());

        let mut errp: c_int = -1;
        let src = unsafe { zip_source_zip_create(za, 0, ZIP_FL_COMPRESSED, 0, -1, &mut errp) };
        assert!(!src.is_null(), "zip_source_zip_create compressed errp={errp}");
        let raw = unsafe { read_source_all(src) };
        unsafe { zip_source_free(src) };

        // The raw compressed stream must not equal the decompressed content.
        let decompressed = b"hello ffi read path content ".repeat(20);
        assert_ne!(raw, decompressed, "compressed != decompressed");
        // And it must not be a valid STORE of the data (it is deflate bytes).
        assert!(!raw.is_empty());
        unsafe { zip_close(za) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4a-5: read/seek/stat/at_eof/is_seekable match C for a file source
    /// and a window source.
    #[test]
    fn source_read_seek_stat_parity() {
        let path = temp_path("src_parity");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let full = std::fs::read(&path).unwrap();

        let mut errp: c_int = -1;
        let src = unsafe { zip_source_file_create(cpath.as_ptr(), 0, -1, &mut errp) };
        assert!(!src.is_null());
        assert_eq!(unsafe { zip_source_is_seekable(src) }, 1, "file is seekable");

        // Seek to an absolute offset and read; verify SEEK_SET/CUR/END.
        assert_eq!(unsafe { zip_source_open(src) }, 0);
        assert_eq!(unsafe { zip_source_seek(src, 100, SEEK_SET) }, 0);
        assert_eq!(unsafe { zip_source_tell(src) }, 100);
        let mut b = [0u8; 16];
        assert_eq!(unsafe { zip_source_read(src, b.as_mut_ptr() as *mut c_void, 16) }, 16);
        assert_eq!(&b, &full[100..116]);
        assert_eq!(unsafe { zip_source_tell(src) }, 116);
        // SEEK_CUR.
        assert_eq!(unsafe { zip_source_seek(src, 10, SEEK_CUR) }, 0);
        assert_eq!(unsafe { zip_source_tell(src) }, 126);
        // SEEK_END: offset is relative to the end (file size).
        assert_eq!(unsafe { zip_source_seek(src, -6, SEEK_END) }, 0);
        assert_eq!(
            unsafe { zip_source_tell(src) },
            (full.len() - 6) as i64
        );
        unsafe { zip_source_close(src) };

        // stat reports size and mtime; at_eof becomes 1 at end.
        let mut sb = zero_stat();
        assert_eq!(unsafe { zip_source_stat(src, &mut sb) }, 0);
        assert_eq!(sb.size, full.len() as u64);
        let drained = unsafe { read_source_all(src) };
        assert_eq!(drained.len(), full.len());
        assert_eq!(unsafe { zip_source_at_eof(src) }, 1);

        unsafe { zip_source_free(src) };
        std::fs::remove_file(&path).ok();
    }

    /// TC-4a-6: no panic on malformed source inputs / callbacks.
    #[test]
    fn source_malformed_no_panic() {
        // A callback returning -1 without setting a source error.
        unsafe extern "C" fn bad_cb(
            _ud: *mut c_void,
            _data: *mut c_void,
            _len: u64,
            _cmd: c_int,
        ) -> i64 {
            -1
        }
        let mut errp: c_int = -1;
        let src = unsafe { zip_source_function_create(Some(bad_cb), std::ptr::null_mut(), &mut errp) };
        assert!(!src.is_null());
        // open/read/seek all return -1, never panic.
        assert_eq!(unsafe { zip_source_open(src) }, -1);
        let mut b = [0u8; 8];
        assert_eq!(unsafe { zip_source_read(src, b.as_mut_ptr() as *mut c_void, 8) }, -1);
        assert_eq!(unsafe { zip_source_seek(src, 0, SEEK_SET) }, -1);
        assert_eq!(unsafe { zip_source_tell(src) }, -1);
        unsafe { zip_source_free(src) };

        // A window with len exceeding the underlying source is clamped, not a panic.
        let path = temp_path("src_mal");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let full = std::fs::read(&path).unwrap();
        let base = unsafe {
            zip_source_file_create(cpath.as_ptr(), 0, -1, std::ptr::null_mut())
        };
        assert!(!base.is_null());
        let win = unsafe {
            zip_source_window_create(base, 0, full.len() as i64 + 100, std::ptr::null_mut())
        };
        assert!(!win.is_null());
        let got = unsafe { read_source_all(win) };
        assert_eq!(got.len(), full.len(), "window clamped to source length");
        unsafe { zip_source_free(win) };

        // zip_source_zip over a deleted / invalid index yields an error, no panic.
        let za = unsafe { zip_open(cpath.as_ptr(), 0, std::ptr::null_mut()) };
        assert!(!za.is_null());
        let mut errp2: c_int = -1;
        let bad = unsafe {
            zip_source_zip_create(za, 9999, 0, 0, -1, &mut errp2)
        };
        assert!(bad.is_null(), "invalid index must fail cleanly");
        unsafe { zip_close(za) };
        std::fs::remove_file(&path).ok();
    }

    /// All 4a C ABI symbols must be exported and callable.
    #[test]
    fn source_symbols_callable() {
        let path = temp_path("src_sym");
        build_zip(&path);
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();

        // zip_source_buffer + zip_source_free already covered elsewhere; here we
        // exercise keep/error/is_deleted/get_file_attributes on a buffer source.
        let data = b"symbol probe bytes".to_vec();
        let src = unsafe {
            zip_source_buffer(std::ptr::null_mut(), data.as_ptr() as *const c_void, data.len() as u64, 0)
        };
        assert!(!src.is_null());
        unsafe { zip_source_keep(src) };
        let errp = unsafe { zip_source_error(src) };
        assert!(!errp.is_null());
        assert_eq!(unsafe { zip_source_is_deleted(src) }, 0);
        let mut attrs = zip_file_attributes {
            valid: 0,
            version: 0,
            host_system: 0,
            ascii: 0,
            version_needed: 0,
            external_file_attributes: 0,
            general_purpose_bit_flags: 0,
            general_purpose_bit_mask: 0,
        };
        // get_file_attributes is not supported for memory sources -> -1.
        assert_eq!(
            unsafe { zip_source_get_file_attributes(src, &mut attrs) },
            -1
        );
        // keep bumped refcount; one free returns (does not double-free).
        unsafe { zip_source_free(src) };
        unsafe { zip_source_free(src) };

        // zip_source_filep_create over a FILE*.
        let cf = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let file = unsafe { libc::fopen(cf.as_ptr(), b"rb".as_ptr() as *const c_char) };
        assert!(!file.is_null());
        let mut errp2: c_int = -1;
        let fsrc = unsafe { zip_source_filep_create(file, 0, -1, &mut errp2) };
        assert!(!fsrc.is_null(), "zip_source_filep_create errp={errp2}");
        let full = std::fs::read(&path).unwrap();
        let got = unsafe { read_source_all(fsrc) };
        assert_eq!(got, full);
        unsafe { zip_source_free(fsrc) };
        unsafe { libc::fclose(file) };

        std::fs::remove_file(&path).ok();
    }
}
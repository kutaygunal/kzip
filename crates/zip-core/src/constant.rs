//! Format constants, signatures, and compression/flag enums for ZIP archives.
//!
//! Values mirror libzip's `zipint.h`/`zip.h` so the FFI layer maps 1:1.

#![allow(missing_docs)] // constants are self-documenting by name

/// PK signature magic bytes.
pub mod magic {
    /// Central directory file header: `PK\x01\x02`.
    pub const CENTRAL: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
    /// Local file header: `PK\x03\x04`.
    pub const LOCAL: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
    /// End of central directory: `PK\x05\x06`.
    pub const EOCD: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    /// Data descriptor: `PK\x07\x08`.
    pub const DATA_DESCRIPTOR: [u8; 4] = [0x50, 0x4B, 0x07, 0x08];
    /// Zip64 end of central directory locator: `PK\x06\x07`.
    pub const EOCD64_LOCATOR: [u8; 4] = [0x50, 0x4B, 0x06, 0x07];
    /// Zip64 end of central directory record: `PK\x06\x06`.
    pub const EOCD64: [u8; 4] = [0x50, 0x4B, 0x06, 0x06];
}

/// Fixed sizes (bytes) of the on-disk structures.
pub mod size {
    /// Central directory entry, fixed part.
    pub const CENTRAL: usize = 46;
    /// Local file header, fixed part.
    pub const LOCAL: usize = 30;
    /// End of central directory record.
    pub const EOCD: usize = 22;
    /// Zip64 EOCD locator.
    pub const EOCD64_LOCATOR: usize = 20;
    /// Zip64 EOCD record.
    pub const EOCD64: usize = 56;
    /// Zip64 extra field, fixed part.
    pub const ZIP64_EXTRA: usize = 28;
}

/// Maximum central-directory tail we read to locate the EOCD (comment + EOCD +
/// zip64 locator, as in libzip's `CDBUFSIZE`).
pub const MAX_CD_BUFFER: usize = 65536 + size::EOCD + size::EOCD64_LOCATOR;
/// Read buffer size for the read path.
pub const BUFFER_SIZE: usize = 8192;

/// Maximum uncompressed entry size (bytes) served by the zero-copy buffered
/// read path. Larger entries fall back to streaming decode to keep memory
/// usage proportional to the read chunk rather than the whole entry.
pub const ZERO_COPY_MAX_UNCOMP: u64 = 32 * 1024 * 1024;

/// Maximum decompressed bytes produced by the zero-copy decode path
/// ([`crate::codec::decode_slice_into`]). Matches `ZERO_COPY_MAX_UNCOMP` so
/// legitimate zero-copy entries (whose claimed size is bounded by that
/// constant) are never rejected, while a malicious small deflate stream
/// cannot expand without bound (zip-bomb guard).
pub const MAX_DECOMPRESSED: u64 = ZERO_COPY_MAX_UNCOMP;

/// Maximum central-directory size (bytes) we will allocate when reading an
/// archive's central directory. Prevents an EOCD/ZIP64 record claiming an
/// absurd `cdir_size` (e.g. `u32::MAX` ≈ 4 GiB) from triggering an unbounded
/// allocation / OOM (zip-bomb guard). Generous enough for all realistic
/// legitimate archives (the largest corpus central directory is ~4 MiB).
pub const MAX_CD_SIZE: u64 = 1 << 30;

/// Number of buffers the archive's [`crate::BufferPool`] will hold for reuse
/// across `open_entry` calls.
pub const BUFFER_POOL_CAPACITY: usize = 8;

/// General purpose bit flag bits (local/central header `bitflags`).
pub mod flag {
    /// Entry is encrypted.
    pub const ENCRYPTED: u16 = 0x0001;
    /// Data descriptor follows the data.
    pub const DATA_DESCRIPTOR: u16 = 0x0008;
    /// Strong encryption.
    pub const STRONG_ENCRYPTION: u16 = 0x0040;
    /// Filename and comment are UTF-8.
    pub const ENCODING_UTF8: u16 = 0x0800;
}

/// Compression methods. Only a subset are implemented for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum CompressionMethod {
    /// Stored (uncompressed).
    Store = 0,
    /// Deflate.
    Deflate = 8,
    /// Bzip2.
    Bzip2 = 12,
    /// Known method that we do not support (value preserved).
    Unsupported(i32),
}

impl CompressionMethod {
    /// Map a stored 16-bit method value to the enum. Unknown values are kept
    /// verbatim so the FFI can report the raw number.
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => CompressionMethod::Store,
            8 => CompressionMethod::Deflate,
            12 => CompressionMethod::Bzip2,
            other => CompressionMethod::Unsupported(other as i32),
        }
    }
}

/// ZIP64 extra field ID.
pub const EXTRA_FIELD_ZIP64: u16 = 0x0001;

/// Value indicating a 32-bit field is 0xFFFFFFFF and the true value is in the
/// ZIP64 extra field.
pub const ZIP64_MAGIC32: u32 = u32::MAX;

/// Encryption method constants, matching libzip's `ZIP_EM_*` values
/// (`zip.h`). `ZIP_EM_NONE` (0) is what an unencrypted entry reports.
pub mod encryption {
    /// No encryption.
    pub const NONE: u16 = 0;
    /// Traditional PKWARE encryption.
    pub const TRAD_PKWARE: u16 = 1;
    /// Unknown / not determinable encryption method.
    pub const UNKNOWN: u16 = 0xFFFF;
}

//! Error model for zip-core.
//!
//! Mirrors libzip's two-axis error space:
//! - **zip error** — a format / API error (enumerated in `ZipErrorCode`).
//! - **system error** — the underlying OS error (`std::io::Error`), when present.
//!
//! The C ABI layer (`zip-sys`) maps these onto `zip_error_t` so existing libzip
//! consumers observe identical error codes.

use std::fmt;

/// A ZIP error code. Mirrors the values from libzip's `zip_err_str.c` /
/// `zipint.h` `ZIP_ER_*` constants so the FFI layer can map 1:1.
///
/// The numeric values are intentional and stable: do not renumber.
///
/// Variant names are self-documenting and map 1:1 to libzip `ZIP_ER_*`;
/// individual doc comments would add noise, so missing-docs is allowed here.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ZipErrorCode {
    Ok = 0,
    /// Multi-disk zip archives not supported.
    Multidisk = 1,
    /// Renaming temporary file failed.
    Rename = 2,
    /// Closing zip archive failed.
    Close = 3,
    /// Seek error.
    Seek = 4,
    /// Read error.
    Read = 5,
    /// Write error.
    Write = 6,
    /// CRC error.
    Crc = 7,
    /// Containing zip archive was closed.
    Zipclosed = 8,
    /// No such file.
    Noent = 9,
    /// File already exists.
    Exists = 10,
    /// Can't open file.
    Open = 11,
    /// Failed to create temporary file.
    Tmpopen = 12,
    /// Zlib error.
    Zlib = 13,
    /// Malloc failure.
    Memory = 14,
    /// Entry has been changed.
    Changed = 15,
    /// Compression method not supported.
    Compnotsupp = 16,
    /// Premature end of file.
    Eof = 17,
    /// Invalid argument.
    Inval = 18,
    /// Not a zip archive.
    Nozip = 19,
    /// Internal error.
    Internal = 20,
    /// Zip archive inconsistent.
    Incons = 21,
    /// Can't remove file.
    Remove = 22,
    /// Entry has been deleted.
    Deleted = 23,
    /// Encryption method not supported.
    Encrmethnotsupp = 24,
    /// Read-only archive.
    Rdonly = 25,
    /// No password provided.
    Nopasswd = 26,
    /// Wrong password provided.
    Wrongpasswd = 27,
    /// Operation not supported.
    Opnotsupp = 28,
    /// Resource still in use.
    Inuse = 29,
    /// Tell error.
    Tell = 30,
    /// Compressed data invalid.
    CompressedData = 31,
    /// Operation cancelled.
    Cancelled = 32,
    /// Entry was not found.
    DataDescriptor = 33,
    /// Zip archive has been destroyed.
    Zipdestroyed = 34,
}

impl ZipErrorCode {
    /// Raw libzip integer value.
    #[inline]
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Map from a raw libzip integer. Unknown values map to `Internal`.
    pub fn from_i32(v: i32) -> Self {
        use ZipErrorCode::*;
        match v {
            0 => Ok,
            1 => Multidisk,
            2 => Rename,
            3 => Close,
            4 => Seek,
            5 => Read,
            6 => Write,
            7 => Crc,
            8 => Zipclosed,
            9 => Noent,
            10 => Exists,
            11 => Open,
            12 => Tmpopen,
            13 => Zlib,
            14 => Memory,
            15 => Changed,
            16 => Compnotsupp,
            17 => Eof,
            18 => Inval,
            19 => Nozip,
            20 => Internal,
            21 => Incons,
            22 => Remove,
            23 => Deleted,
            24 => Encrmethnotsupp,
            25 => Rdonly,
            26 => Nopasswd,
            27 => Wrongpasswd,
            28 => Opnotsupp,
            29 => Inuse,
            30 => Tell,
            31 => CompressedData,
            32 => Cancelled,
            33 => DataDescriptor,
            34 => Zipdestroyed,
            _ => Internal,
        }
    }
}

/// A full error carrying both a zip error code and an optional system error.
///
/// This mirrors libzip's `zip_error_t` semantics: the zip error always exists,
/// and the system error layer is optional (populated only for OS-level failures
/// like read/write/seek errors).
#[derive(Debug)]
#[allow(missing_docs)] // fields self-explanatory
pub struct ZipError {
    pub code: ZipErrorCode,
    pub system: Option<std::io::Error>,
}

impl ZipError {
    /// Construct an error with only a zip error code (no system error).
    pub fn new(code: ZipErrorCode) -> Self {
        ZipError { code, system: None }
    }

    /// Construct an error carrying both a zip code and a system error.
    pub fn with_system(code: ZipErrorCode, err: std::io::Error) -> Self {
        ZipError {
            code,
            system: Some(err),
        }
    }

    /// The primary zip error code.
    pub fn code(&self) -> ZipErrorCode {
        self.code
    }

    /// The OS error string, if present (owned copy).
    pub fn system_message(&self) -> Option<String> {
        self.system.as_ref().map(|e| e.to_string())
    }
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.code)?;
        if let Some(sys) = &self.system {
            write!(f, ": {sys}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ZipError {}

impl From<ZipErrorCode> for ZipError {
    fn from(code: ZipErrorCode) -> Self {
        ZipError::new(code)
    }
}

impl From<std::io::Error> for ZipError {
    fn from(err: std::io::Error) -> Self {
        // io::Error doesn't cleanly map to a zip code; callers usually wrap a
        // specific code (Read/Write/Seek) before conversion. This fallback
        // uses Internal and preserves the message.
        ZipError {
            code: ZipErrorCode::Internal,
            system: Some(err),
        }
    }
}

/// Convenience result alias used across the crate.
pub type Result<T> = std::result::Result<T, ZipError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_codes() {
        for i in 0..=34 {
            let c = ZipErrorCode::from_i32(i);
            assert_eq!(c.as_i32(), i, "mismatch at {i}");
        }
    }

    #[test]
    fn unknown_code_maps_to_internal() {
        assert_eq!(ZipErrorCode::from_i32(999), ZipErrorCode::Internal);
        assert_eq!(ZipErrorCode::from_i32(-5), ZipErrorCode::Internal);
    }

    #[test]
    fn display_contains_code_name() {
        let e = ZipError::new(ZipErrorCode::Crc);
        assert!(e.to_string().contains("Crc"));
    }

    #[test]
    fn io_error_conversion_preserves_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let e: ZipError = io.into();
        assert_eq!(e.code(), ZipErrorCode::Internal);
        assert!(e.to_string().contains("missing file"));
    }
}

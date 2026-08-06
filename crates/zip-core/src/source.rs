//! The layered `Source` pipeline trait.
//!
//! This mirrors libzip's `zip_source_t` abstraction: every input/output is a
//! node in a stack of layers (buffer → window → crc → compress → encrypt).
//!
//! In C this is a command-switch (`SRC_OPEN`, `SRC_READ`, ...). In Rust we model
//! the same capabilities with trait methods. A `Source` is a byte provider:
//! callers pull bytes via `read`, and a layered decorator transforms the stream
//! (e.g. decompress, decrypt). Writers are handled by `WriteSource`.

use crate::error::{Result, ZipError, ZipErrorCode};
use std::io::{Read, Seek, SeekFrom};

/// Metadata about a source/entry, mirroring libzip's `zip_stat_t`.
/// Field names are self-explanatory; missing-docs allowed to reduce noise.
#[allow(missing_docs)]
#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub index: Option<u64>,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub comp_size: Option<u64>,
    pub mtime: Option<u64>,
    pub crc: Option<u32>,
    pub comp_method: Option<u16>,
    pub encryption_method: Option<u16>,
    pub valid: u64,
}

/// How a source may be used. Mirrors `ZIP_SOURCE_SUPPORTS_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supports {
    /// Readable, not seekable.
    Readable,
    /// Seekable, not readable (rare).
    Seekable,
    /// Writable, requires seeking.
    Writable,
    /// Writable, sequential only.
    SequentialWrite,
    /// Both readable and seekable (most common).
    ReadableAndSeekable,
}

impl Supports {
    /// Does this source support read operations?
    #[inline]
    pub fn has_read(self) -> bool {
        matches!(self, Supports::Readable | Supports::ReadableAndSeekable)
    }
    /// Does this source support seeking?
    #[inline]
    pub fn has_seek(self) -> bool {
        matches!(self, Supports::Seekable | Supports::ReadableAndSeekable)
    }
    /// Does this source support writing?
    #[inline]
    pub fn has_write(self) -> bool {
        matches!(self, Supports::Writable | Supports::SequentialWrite)
    }
}

/// A source of bytes: readable and possibly seekable.
///
/// This is the primary trait for the *read / decode* path. A concrete source
/// (file, buffer, another zip entry) implements this; layered decorators wrap
/// another `Source` and transform the byte stream (e.g. decompress, decrypt).
///
/// `duplicate()` produces an independent handle positioned at the start. It is
/// used by the archive to open per-entry readers from a single underlying
/// source (a `File` clones its handle; a `Cursor` clones its buffer).
pub trait Source: Read + Seek + Send + Sync {
    /// Advertised capabilities (used for `zip_source_is_seekable` etc.).
    fn supports(&self) -> Supports;

    /// Whether the underlying data is a real file (affects some write paths).
    fn is_seekable(&self) -> bool {
        self.supports().has_seek()
    }

    /// Stat metadata for this source, if known.
    fn stat(&self) -> Result<Stat> {
        Ok(Stat::default())
    }

    /// Whether this source is a duplicate/deleted placeholder.
    fn is_deleted(&self) -> bool {
        false
    }

    /// If this source is an in-memory buffer owned contiguously, return a slice
    /// of its **entire** backing data. The zero-copy read path uses this to
    /// avoid copying the buffer before decoding. Default: `None`.
    fn as_slice(&self) -> Option<&[u8]> {
        None
    }

    /// Create an independent, freshly-positioned copy of this source.
    fn duplicate(&self) -> Result<Box<dyn Source>>;

    /// Create an independent copy of this source positioned at `offset`.
    ///
    /// The default implementation calls [`duplicate`] (which positions the
    /// clone at the start) and then seeks to `offset`. Sources that can clone
    /// and seek in a single step (e.g. a `File`, where the wasted `seek(0)` in
    /// [`duplicate`] is immediately overwritten by the caller) override this to
    /// avoid the redundant syscall on the per-entry read-open path.
    fn duplicate_at(&self, offset: u64) -> Result<Box<dyn Source>> {
        let mut d = self.duplicate()?;
        d.seek(SeekFrom::Start(offset))
            .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
        Ok(d)
    }
}

impl Source for std::fs::File {
    fn supports(&self) -> Supports {
        Supports::ReadableAndSeekable
    }

    fn duplicate(&self) -> Result<Box<dyn Source>> {
        let mut f = self
            .try_clone()
            .map_err(|e| ZipError::with_system(ZipErrorCode::Open, e))?;
        // Position the clone at the start so each duplicate is a freshly
        // positioned handle. On Windows, duplicated file handles share a single
        // OS file-position pointer, so the explicit seek is required to honor
        // the "positioned at the start" contract.
        f.seek(SeekFrom::Start(0))
            .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
        Ok(Box::new(f))
    }

    fn duplicate_at(&self, offset: u64) -> Result<Box<dyn Source>> {
        let mut f = self
            .try_clone()
            .map_err(|e| ZipError::with_system(ZipErrorCode::Open, e))?;
        // Clone + seek in one step: skip the redundant `seek(0)` that
        // `duplicate()` performs and that the caller would immediately
        // overwrite with `offset`.
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| ZipError::with_system(ZipErrorCode::Seek, e))?;
        Ok(Box::new(f))
    }
}

impl Source for std::io::Cursor<Vec<u8>> {
    fn supports(&self) -> Supports {
        Supports::ReadableAndSeekable
    }

    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.get_ref().as_slice())
    }

    fn duplicate(&self) -> Result<Box<dyn Source>> {
        Ok(Box::new(std::io::Cursor::new(self.get_ref().clone())))
    }
}

impl Source for std::io::Cursor<Box<[u8]>> {
    fn supports(&self) -> Supports {
        Supports::ReadableAndSeekable
    }

    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.get_ref())
    }

    fn duplicate(&self) -> Result<Box<dyn Source>> {
        Ok(Box::new(std::io::Cursor::new(
            self.get_ref().to_vec().into_boxed_slice(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, SeekFrom};

    #[test]
    fn cursor_is_seekable_source() {
        let cur = Cursor::new(vec![1u8, 2, 3]);
        assert!(cur.is_seekable());
        assert!(cur.supports().has_read());
    }

    #[test]
    fn supports_flags_work() {
        assert!(Supports::Readable.has_read());
        assert!(!Supports::Readable.has_seek());
        assert!(Supports::ReadableAndSeekable.has_read());
        assert!(Supports::ReadableAndSeekable.has_seek());
        assert!(Supports::Writable.has_write());
    }

    #[test]
    fn read_from_cursor_source() {
        let mut cur = Cursor::new(vec![9u8, 8, 7]);
        let mut buf = [0u8; 2];
        cur.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[9, 8]);
        cur.seek(SeekFrom::Start(0)).unwrap();
        cur.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &[9, 8]);
    }

    #[test]
    fn file_duplicate_is_independent() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipcore_dup_{}.bin", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[1, 2, 3, 4, 5]).unwrap();
        drop(f);

        let mut f = std::fs::File::open(&path).unwrap();
        // Advance the original; `duplicate()` must still return a handle that
        // reads from the start of the file.
        f.seek(SeekFrom::Start(3)).unwrap();
        let mut dup = f.duplicate().unwrap();
        let mut b = [0u8; 2];
        dup.read_exact(&mut b).unwrap();
        assert_eq!(&b, &[1, 2]);
        std::fs::remove_file(&path).ok();
    }
}

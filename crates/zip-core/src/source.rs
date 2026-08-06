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
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

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

    /// If this source is backed by a real file, return a shared, mutex-
    /// protected handle to it.
    ///
    /// The archive uses this to open per-entry readers from a single shared
    /// handle instead of cloning the OS handle per entry (`try_clone` /
    /// `DuplicateHandle`, the single most expensive syscall on the read-open
    /// path). Each reader tracks its own logical position and serializes its
    /// reads on the shared handle's mutex. Default: `None` (non-file sources).
    fn shared_handle(&self) -> Option<Arc<Mutex<SharedFileState>>> {
        None
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

    fn shared_handle(&self) -> Option<Arc<Mutex<SharedFileState>>> {
        // Clone the OS handle once (at archive open), then share it across all
        // per-entry readers. This is the single `DuplicateHandle` that replaces
        // the per-entry `try_clone` on the read-open path.
        let file = self.try_clone().ok()?;
        // `last_pos` is a sentinel (`u64::MAX`) so the first read always seeks:
        // on Windows a duplicated handle shares the OS file pointer with the
        // original, which `read_central_dir` has already moved, so the shared
        // handle's actual position is unknown (not 0) at creation.
        Some(Arc::new(Mutex::new(SharedFileState {
            file,
            last_pos: u64::MAX,
        })))
    }
}

/// Shared mutable state for a [`SharedFile`]: the underlying file plus the
/// last position the shared OS file pointer was left at. A reader skips the
/// seek when the shared pointer is already where it needs to be (the common
/// sequential single-reader case), and re-seeks when another reader moved it
/// (concurrent readers), keeping the shared handle correct for both.
pub struct SharedFileState {
    pub(crate) file: std::fs::File,
    pub(crate) last_pos: u64,
}

impl std::fmt::Debug for SharedFileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFileState")
            .field("last_pos", &self.last_pos)
            .finish_non_exhaustive()
    }
}

/// A `Source` backed by a shared, mutex-protected file handle.
///
/// Multiple readers created from the same archive share one OS file handle
/// (no per-entry `try_clone`/`DuplicateHandle`). Each reader keeps its own
/// logical position `pos`; every read locks the shared handle, seeks only if
/// the shared pointer is not already at `pos`, reads, and advances `pos`.
/// This is correct for concurrent readers (they serialize on the mutex and
/// re-seek to their own position) and cheap for the sequential case (no
/// redundant seek once the pointer is already in place).
pub struct SharedFile {
    inner: Arc<Mutex<SharedFileState>>,
    pos: u64,
}

impl std::fmt::Debug for SharedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFile")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl SharedFile {
    /// Create a reader over the shared handle positioned at `offset`.
    pub fn at_offset(inner: Arc<Mutex<SharedFileState>>, offset: u64) -> Self {
        SharedFile { inner, pos: offset }
    }
}

impl Read for SharedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "shared file lock poisoned"))?;
        if state.last_pos != self.pos {
            state.file.seek(SeekFrom::Start(self.pos))?;
            state.last_pos = self.pos;
        }
        let n = state.file.read(buf)?;
        self.pos += n as u64;
        state.last_pos = self.pos;
        Ok(n)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            SeekFrom::End(d) => {
                let state = self
                    .inner
                    .lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "shared file lock poisoned"))?;
                let len = state.file.metadata()?.len();
                (len as i64 + d).max(0) as u64
            }
        };
        self.pos = new_pos;
        Ok(new_pos)
    }
}

impl Source for SharedFile {
    fn supports(&self) -> Supports {
        Supports::ReadableAndSeekable
    }

    fn duplicate(&self) -> Result<Box<dyn Source>> {
        // Share the same handle; no OS handle clone. Positioned at the start.
        Ok(Box::new(SharedFile {
            inner: self.inner.clone(),
            pos: 0,
        }))
    }

    fn duplicate_at(&self, offset: u64) -> Result<Box<dyn Source>> {
        // Share the same handle; no OS handle clone. Positioned at `offset`.
        Ok(Box::new(SharedFile {
            inner: self.inner.clone(),
            pos: offset,
        }))
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

    /// A `SharedFile` reader reads from its own logical position, and two
    /// readers sharing one handle can interleave correctly (each re-seeks to
    /// its own position).
    #[test]
    fn shared_file_readers_are_independent() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zipcore_shared_{}.bin", std::process::id()));
        let data: Vec<u8> = (0..100u8).collect();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&data).unwrap();
        drop(f);

        let f = std::fs::File::open(&path).unwrap();
        let shared = f.shared_handle().expect("file exposes shared handle");

        // Two readers at different offsets over the same shared handle.
        let mut a = SharedFile::at_offset(shared.clone(), 10);
        let mut b = SharedFile::at_offset(shared.clone(), 50);
        let mut ba = [0u8; 4];
        let mut bb = [0u8; 4];
        // Interleave reads: each must read from its own position.
        a.read_exact(&mut ba).unwrap();
        assert_eq!(&ba, &data[10..14]);
        b.read_exact(&mut bb).unwrap();
        assert_eq!(&bb, &data[50..54]);
        // Second reads advance each reader's own position.
        a.read_exact(&mut ba).unwrap();
        assert_eq!(&ba, &data[14..18]);
        b.read_exact(&mut bb).unwrap();
        assert_eq!(&bb, &data[54..58]);

        // duplicate_at shares the handle and positions at the given offset.
        let mut c = a.duplicate_at(20).unwrap();
        let mut bc = [0u8; 3];
        c.read_exact(&mut bc).unwrap();
        assert_eq!(&bc, &data[20..23]);

        std::fs::remove_file(&path).ok();
    }
}

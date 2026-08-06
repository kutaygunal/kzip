//! # zip-core
//!
//! Safe Rust core engine for reading, creating, and modifying ZIP archives —
//! the heart of the kzip project.
//!
//! `zip-core` is a from-scratch Rust port of [libzip](https://libzip.org/)
//! that prioritizes **correctness and ZIP-format conformance**, memory safety,
//! and panic-free behavior on malformed input. It adds three capabilities the C
//! library lacks out of the box:
//!
//! 1. **Parallel compression** — compress independent archive files
//!    concurrently (feature `parallel`, default on) with byte-identical,
//!    deterministic output.
//! 2. **Zero-copy I/O** — avoid copying buffers on the read/decode path
//!    ([`BufferPool`], [`decode_slice_into`]).
//! 3. **Async streaming** — non-blocking archive read/write via the sibling
//!    `zip-async` crate.
//!
//! A separate `zip-sys` crate exposes the same `zip_*` C ABI as libzip for
//! drop-in compatibility of existing consumers.
//!
//! ## Example — compress and decode
//!
//! ```
//! use zip_core::{
//!     compress_bytes, compress_files, decode_slice_into, ArchiveFile, CompressOptions,
//! };
//! use zip_core::constant::CompressionMethod;
//!
//! // Parallel compression of independent files (deterministic, byte-identical
//! // to serial mode).
//! let files = vec![
//!     ArchiveFile::new("a.txt", b"hello world".to_vec()),
//!     ArchiveFile::new("b.txt", b"goodbye world".to_vec()),
//! ];
//! let opts = CompressOptions::default();
//! let compressed = compress_files(&files, &opts).unwrap();
//! assert_eq!(compressed.len(), 2);
//!
//! // Or compress a single buffer and decode it back (zero-copy decode path).
//! let data = b"some payload that round-trips through DEFLATE".to_vec();
//! let bytes = compress_bytes(&data, CompressionMethod::Deflate, 6).unwrap();
//! let mut out = Vec::new();
//! decode_slice_into(&bytes, CompressionMethod::Deflate, data.len() as u64, &mut out).unwrap();
//! assert_eq!(out, data);
//! ```
//!
//! ## Feature flags
//!
//! - `parallel` *(default)* — parallel compression of independent files via
//!   rayon. Disable for a minimal, dependency-light build.
//!
//! ## License
//!
//! BSD-3-Clause (matching libzip). See the workspace `LICENSE`.

#![deny(unsafe_code)]
#![deny(missing_debug_implementations)]
#![warn(missing_docs)]

pub mod archive;
pub mod bufferpool;
pub mod cdir;
pub mod codec;
pub mod compress;
pub mod constant;
pub mod crypto;
pub mod dirent;
pub mod error;
pub mod file;
pub mod reader;
pub mod source;

pub use archive::Archive;
pub use bufferpool::BufferPool;
pub use codec::decode_slice_into;
pub use compress::{
    compress_bytes, compress_files, write_archive, write_archive_encrypted,
    write_archive_encrypted_methods, write_archive_full, write_archive_full_with_progress,
    ArchiveFile, CompressOptions, CompressedFile,
};
pub use error::{Result, ZipError, ZipErrorCode};
pub use file::EntryReader;
pub use source::{Source, Stat, Supports};

//! Compression (write path) with optional parallel execution.
//!
//! Each archive file is compressed **independently** — its compressed bytes,
//! CRC, and local header do not depend on any other file. That makes the set of
//! files embarrassingly parallel. We use a bounded, work-stealing rayon pool
//! (feature `parallel`) and then emit results **in input order**, so the output
//! is byte-identical to serial mode (deterministic).
//!
//! The codecs themselves are serial (flate2/bzip2-rs are single-threaded
//! per-stream), so parallelism comes from compressing many files at once, not
//! from splitting one file. A single very large file therefore provides no
//! parallelism win and, if it monopolized a worker, would hurt the whole pool —
//! so such files are compressed serially (fall back).

use crate::constant::{flag, magic, CompressionMethod};
use crate::error::{Result, ZipError, ZipErrorCode};
use std::io::Write;

/// A single archive file's raw content, ready for compression.
#[derive(Debug, Clone)]
pub struct ArchiveFile {
    /// Member name stored in the archive (may contain path separators).
    pub name: String,
    /// Uncompressed content.
    pub data: Vec<u8>,
}

impl ArchiveFile {
    /// Create a new archive file.
    pub fn new(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        ArchiveFile {
            name: name.into(),
            data: data.into(),
        }
    }
}

/// Compression / scheduling settings for a batch of files.
#[derive(Debug, Clone, Copy)]
pub struct CompressOptions {
    /// Compression method. Only `Store` and `Deflate` are implemented here.
    pub method: CompressionMethod,
    /// Deflate level 0..=9 (ignored for `Store`).
    pub level: u32,
    /// Enable parallel compression across independent files.
    pub parallel: bool,
    /// Number of workers for the rayon pool. `0` = auto (one per core).
    pub workers: usize,
    /// Files at or above this size (bytes) are compressed serially.
    pub large_file_threshold: u64,
}

impl Default for CompressOptions {
    fn default() -> Self {
        CompressOptions {
            method: CompressionMethod::Deflate,
            level: 6,
            parallel: true,
            workers: 0,
            large_file_threshold: 8 * 1024 * 1024,
        }
    }
}

/// The result of compressing one file: everything needed to emit its local
/// header, data, and central-directory record.
#[derive(Debug, Clone)]
pub struct CompressedFile {
    /// Member name.
    pub name: String,
    /// Stored compression method (u16 on-disk value).
    pub method: u16,
    /// CRC-32 of the uncompressed content.
    pub crc: u32,
    /// Compressed byte length.
    pub comp_size: u64,
    /// Uncompressed byte length.
    pub uncomp_size: u64,
    /// Compressed bytes.
    pub data: Vec<u8>,
}

/// Compress a single byte slice. Deterministic given `(method, level, input)`.
pub fn compress_bytes(data: &[u8], method: CompressionMethod, level: u32) -> Result<Vec<u8>> {
    match method {
        CompressionMethod::Store => Ok(data.to_vec()),
        CompressionMethod::Deflate => {
            let lvl = flate2::Compression::new(level.min(9));
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), lvl);
            enc.write_all(data)
                .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))?;
            enc.finish()
                .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))
        }
        _ => Err(ZipError::new(ZipErrorCode::Compnotsupp)),
    }
}

/// Compress one archive file.
pub fn compress_file(file: &ArchiveFile, opts: &CompressOptions) -> Result<CompressedFile> {
    let data = compress_bytes(&file.data, opts.method, opts.level)?;
    Ok(CompressedFile {
        name: file.name.clone(),
        method: method_to_u16(opts.method),
        crc: crc32fast::hash(&file.data),
        comp_size: data.len() as u64,
        uncomp_size: file.data.len() as u64,
        data,
    })
}

/// Compress a batch of files, optionally in parallel, returning results in
/// **input order** (byte-identical to serial mode).
pub fn compress_files(
    files: &[ArchiveFile],
    opts: &CompressOptions,
) -> Result<Vec<CompressedFile>> {
    let has_large = files
        .iter()
        .any(|f| f.data.len() as u64 >= opts.large_file_threshold);
    let use_parallel = opts.parallel && files.len() > 1 && !has_large;

    if use_parallel {
        #[cfg(feature = "parallel")]
        {
            parallel_compress(files, opts)
        }
        #[cfg(not(feature = "parallel"))]
        {
            files.iter().map(|f| compress_file(f, opts)).collect()
        }
    } else {
        files.iter().map(|f| compress_file(f, opts)).collect()
    }
}

/// Compress `files` concurrently on a bounded rayon pool, collecting results
/// into per-file slots emitted in index order for deterministic output.
#[cfg(feature = "parallel")]
fn parallel_compress(files: &[ArchiveFile], opts: &CompressOptions) -> Result<Vec<CompressedFile>> {
    use rayon::prelude::*;

    let workers = if opts.workers == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        opts.workers
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .map_err(|_| ZipError::new(ZipErrorCode::Memory))?;

    // `par_iter` on a slice is an indexed parallel iterator: `collect` preserves
    // the source order, and each element is computed independently, so the
    // resulting sequence is identical to serial mode.
    pool.install(|| {
        files
            .par_iter()
            .map(|f| compress_file(f, opts))
            .collect::<Result<Vec<CompressedFile>>>()
    })
}

fn method_to_u16(m: CompressionMethod) -> u16 {
    match m {
        CompressionMethod::Store => 0,
        CompressionMethod::Deflate => 8,
        CompressionMethod::Bzip2 => 12,
        CompressionMethod::Unsupported(v) => v as u16,
    }
}

// ---------------------------------------------------------------------------
// ArchiveWriter: emit a complete, valid ZIP archive from compressed files.
// Used by integration tests and the async adapter's read round-trip. This is a
// minimal, deterministic writer (no ZIP64, fixed DOS time of 0, no encryption).
// ---------------------------------------------------------------------------

/// Write a complete ZIP archive containing `files`, compressed per `opts`.
///
/// The layout is local-file-header + data for each member (in order), then the
/// central directory, then the EOCD record. The produced bytes can be re-read
/// with [`crate::Archive::open`].
pub fn write_archive(files: &[ArchiveFile], opts: &CompressOptions) -> Result<Vec<u8>> {
    let compressed = compress_files(files, opts)?;
    let mut out = Vec::new();
    let mut cdir = Vec::new();

    for cf in &compressed {
        let offset = out.len() as u64;
        write_local_header(cf, &mut out);
        out.extend_from_slice(&cf.data);
        write_central_entry(cf, offset, &mut cdir);
    }

    let cdir_offset = out.len() as u64;
    out.extend_from_slice(&cdir);
    let cdir_size = cdir.len() as u64;
    write_eocd(compressed.len() as u16, cdir_size, cdir_offset, &mut out);
    Ok(out)
}

fn write_local_header(cf: &CompressedFile, out: &mut Vec<u8>) {
    out.extend_from_slice(&magic::LOCAL);
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // bit flags (sizes known)
    out.extend_from_slice(&cf.method.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // dos time/date = 0
    out.extend_from_slice(&cf.crc.to_le_bytes());
    out.extend_from_slice(&(cf.comp_size as u32).to_le_bytes());
    out.extend_from_slice(&(cf.uncomp_size as u32).to_le_bytes());
    out.extend_from_slice(&(cf.name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra len
    out.extend_from_slice(cf.name.as_bytes());
}

fn write_central_entry(cf: &CompressedFile, offset: u64, cdir: &mut Vec<u8>) {
    cdir.extend_from_slice(&magic::CENTRAL);
    cdir.extend_from_slice(&(3u16 << 8 | 20u16).to_le_bytes()); // version made by: unix, 2.0
    cdir.extend_from_slice(&20u16.to_le_bytes()); // version needed
    cdir.extend_from_slice(&0u16.to_le_bytes()); // bit flags
    cdir.extend_from_slice(&cf.method.to_le_bytes());
    cdir.extend_from_slice(&0u32.to_le_bytes()); // dos time/date
    cdir.extend_from_slice(&cf.crc.to_le_bytes());
    cdir.extend_from_slice(&(cf.comp_size as u32).to_le_bytes());
    cdir.extend_from_slice(&(cf.uncomp_size as u32).to_le_bytes());
    cdir.extend_from_slice(&(cf.name.len() as u16).to_le_bytes());
    cdir.extend_from_slice(&0u16.to_le_bytes()); // extra len
    cdir.extend_from_slice(&0u16.to_le_bytes()); // comment len
    cdir.extend_from_slice(&0u16.to_le_bytes()); // disk number
    cdir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    cdir.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    cdir.extend_from_slice(&(offset as u32).to_le_bytes());
    cdir.extend_from_slice(cf.name.as_bytes());
}

fn write_eocd(num_entries: u16, cdir_size: u64, cdir_offset: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&magic::EOCD);
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with cdir
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&(cdir_size as u32).to_le_bytes());
    out.extend_from_slice(&(cdir_offset as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
}

// Silence unused-import warning when the flag module is only used by future
// encryption work; it is still referenced in doc comments.
#[allow(unused)]
fn _flag_ref() -> u16 {
    flag::ENCRYPTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use std::io::{Cursor, Read};

    fn sample_files() -> Vec<ArchiveFile> {
        vec![
            ArchiveFile::new("a/one.txt", b"hello one file, some content here".repeat(40)),
            ArchiveFile::new("b/two.bin", vec![0u8; 4096]),
            ArchiveFile::new(
                "c/three.txt",
                b"third file with compressible text ".repeat(200),
            ),
            ArchiveFile::new(
                "d/four.bin",
                (0u8..=255).cycle().take(8192).collect::<Vec<u8>>(),
            ),
        ]
    }

    #[test]
    fn serial_compresses_deterministically() {
        let files = sample_files();
        let opts = CompressOptions {
            parallel: false,
            ..Default::default()
        };
        let a = compress_files(&files, &opts).unwrap();
        let b = compress_files(&files, &opts).unwrap();
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.data, y.data, "serial output must be deterministic");
        }
    }

    #[test]
    fn parallel_matches_serial_byte_for_byte() {
        let files = sample_files();
        let serial = CompressOptions {
            parallel: false,
            ..Default::default()
        };
        let par = CompressOptions {
            parallel: true,
            workers: 2,
            ..Default::default()
        };
        let s = compress_files(&files, &serial).unwrap();
        let p = compress_files(&files, &par).unwrap();
        assert_eq!(s.len(), p.len());
        for (x, y) in s.iter().zip(p.iter()) {
            assert_eq!(x.crc, y.crc);
            assert_eq!(x.comp_size, y.comp_size);
            assert_eq!(x.uncomp_size, y.uncomp_size);
            assert_eq!(
                x.data, y.data,
                "parallel output must be byte-identical to serial"
            );
        }
        assert_eq!(s[0].name, "a/one.txt");
        assert_eq!(s[3].name, "d/four.bin");
    }

    #[test]
    fn large_file_falls_back_to_serial() {
        // A single file above the threshold must still produce identical output
        // to serial (and not panic / spawn a parallel pool for one file).
        let big = vec![b'x'; 64];
        let files = vec![ArchiveFile::new("big.bin", big)];
        let opts = CompressOptions {
            parallel: true,
            large_file_threshold: 32,
            ..Default::default()
        };
        let out = compress_files(&files, &opts).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].comp_size < out[0].uncomp_size); // deflate worked
    }

    #[test]
    fn store_is_passthrough() {
        let files = vec![ArchiveFile::new("raw.bin", vec![1, 2, 3, 4])];
        let opts = CompressOptions {
            method: CompressionMethod::Store,
            ..Default::default()
        };
        let out = compress_files(&files, &opts).unwrap();
        assert_eq!(out[0].data, vec![1, 2, 3, 4]);
        assert_eq!(out[0].comp_size, 4);
        assert_eq!(out[0].uncomp_size, 4);
    }

    #[test]
    fn parallel_deterministic_across_worker_counts() {
        // The whole point of the parallel path is byte-identical output: the
        // same batch must compress to the exact same bytes regardless of how
        // many rayon workers are used.
        let files = sample_files();
        let serial = CompressOptions {
            parallel: false,
            ..Default::default()
        };
        let s = compress_files(&files, &serial).unwrap();
        for workers in [1usize, 2, 3, 4, 8] {
            let par = CompressOptions {
                parallel: true,
                workers,
                ..Default::default()
            };
            let p = compress_files(&files, &par).unwrap();
            assert_eq!(p.len(), s.len(), "worker={workers}: entry count changed");
            for (x, y) in s.iter().zip(p.iter()) {
                assert_eq!(x.crc, y.crc, "worker={workers}: crc diverged");
                assert_eq!(
                    x.comp_size, y.comp_size,
                    "worker={workers}: comp_size diverged"
                );
                assert_eq!(
                    x.uncomp_size, y.uncomp_size,
                    "worker={workers}: uncomp_size diverged"
                );
                assert_eq!(
                    x.data, y.data,
                    "worker={workers}: compressed bytes diverged from serial"
                );
            }
        }
    }

    #[test]
    fn large_file_forces_whole_batch_serial_but_identical() {
        // A single large file among many small ones short-circuits the whole
        // batch to serial; the output must still be byte-identical to a fully
        // serial run (no ordering or CRC drift).
        let mut files = sample_files();
        files.insert(1, ArchiveFile::new("big.bin", vec![b'z'; 48]));
        let serial = CompressOptions {
            parallel: false,
            large_file_threshold: 32,
            ..Default::default()
        };
        let par = CompressOptions {
            parallel: true,
            workers: 4,
            large_file_threshold: 32,
            ..Default::default()
        };
        let s = compress_files(&files, &serial).unwrap();
        let p = compress_files(&files, &par).unwrap();
        assert_eq!(s.len(), p.len());
        for (x, y) in s.iter().zip(p.iter()) {
            assert_eq!(
                x.data, y.data,
                "mixed batch with a large file must be serial-identical"
            );
            assert_eq!(x.name, y.name);
        }
    }

    #[test]
    fn single_file_never_takes_parallel_path() {
        // `files.len() > 1` guards the parallel path, so a lone file (even a
        // small one with `parallel=true`) must match serial exactly.
        let files = vec![ArchiveFile::new(
            "only.txt",
            b"single small file payload ".repeat(50),
        )];
        let par = CompressOptions {
            parallel: true,
            workers: 4,
            ..Default::default()
        };
        let serial = CompressOptions {
            parallel: false,
            ..Default::default()
        };
        let p = compress_files(&files, &par).unwrap();
        let s = compress_files(&files, &serial).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].data, s[0].data);
        assert_eq!(p[0].crc, s[0].crc);
        assert_eq!(p[0].comp_size, s[0].comp_size);
    }

    #[test]
    fn zero_threshold_treats_everything_as_large() {
        // A threshold of 0 means every file is "large", so parallel is never
        // used; output must still be deterministic and identical to serial.
        let files = sample_files();
        let serial = CompressOptions {
            parallel: false,
            large_file_threshold: 0,
            ..Default::default()
        };
        let par = CompressOptions {
            parallel: true,
            workers: 4,
            large_file_threshold: 0,
            ..Default::default()
        };
        let s = compress_files(&files, &serial).unwrap();
        let p = compress_files(&files, &par).unwrap();
        assert_eq!(s.len(), p.len());
        for (x, y) in s.iter().zip(p.iter()) {
            assert_eq!(x.data, y.data);
            assert_eq!(x.crc, y.crc);
        }
    }

    #[test]
    fn store_parallel_is_deterministic() {
        // Store is a passthrough; the parallel path must not reorder or alter
        // stored entries.
        let files = sample_files();
        let serial = CompressOptions {
            method: CompressionMethod::Store,
            parallel: false,
            ..Default::default()
        };
        let par = CompressOptions {
            method: CompressionMethod::Store,
            parallel: true,
            workers: 3,
            ..Default::default()
        };
        let s = compress_files(&files, &serial).unwrap();
        let p = compress_files(&files, &par).unwrap();
        assert_eq!(s.len(), p.len());
        for (x, y) in s.iter().zip(p.iter()) {
            assert_eq!(x.data, y.data);
            assert_eq!(x.name, y.name);
        }
    }

    #[test]
    fn empty_file_list_produces_empty_output() {
        let files: Vec<ArchiveFile> = Vec::new();
        let opts = CompressOptions {
            parallel: true,
            ..Default::default()
        };
        let out = compress_files(&files, &opts).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn write_and_reopen_roundtrip() {
        let files = sample_files();
        let opts = CompressOptions {
            parallel: true,
            workers: 2,
            ..Default::default()
        };
        let bytes = write_archive(&files, &opts).unwrap();

        let arch = Archive::open(Cursor::new(bytes)).unwrap();
        assert_eq!(arch.len(), 4);
        for (i, f) in files.iter().enumerate() {
            assert_eq!(arch.name(i as u64), Some(f.name.as_str()));
            let mut r = arch.open_entry(i as u64).unwrap();
            let mut out = Vec::new();
            r.read_to_end(&mut out).unwrap();
            assert_eq!(out, f.data, "roundtrip content mismatch for {}", f.name);
        }
    }
}

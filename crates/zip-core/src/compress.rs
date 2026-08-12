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

use crate::constant::{
    flag, magic, CompressionMethod, EF_WINZIP_AES, EF_WINZIP_AES_SIZE, EXTRA_FIELD_ZIP64,
    ZIP_CM_WINZIP_AES,
};
use crate::error::{Result, ZipError, ZipErrorCode};
use std::io::Write;

/// A single archive file's raw content, ready for compression.
///
/// In addition to name/data this carries the per-entry metadata the write path
/// must emit: an entry comment, user extra fields, DOS last-mod time/date,
/// external (host-specific) attributes, and an optional per-entry compression
/// override. Every metadata field has a safe default so `ArchiveFile::new`
/// (used throughout the codebase) keeps its existing semantics.
#[derive(Debug, Clone)]
pub struct ArchiveFile {
    /// Member name stored in the archive (may contain path separators).
    pub name: String,
    /// Uncompressed content.
    pub data: Vec<u8>,
    /// Entry comment (raw bytes), or `None` for no comment.
    pub comment: Option<Vec<u8>>,
    /// User extra fields `(id, data)`. Internal ZIP64/AES fields are excluded
    /// by the writer (it manages them itself).
    pub extra_fields: Vec<(u16, Vec<u8>)>,
    /// DOS-encoded last modification time.
    pub last_mod_time: u16,
    /// DOS-encoded last modification date.
    pub last_mod_date: u16,
    /// Host system (upper byte of "version made by"; e.g. `ZIP_OPSYS_UNIX`=3).
    pub opsys: u8,
    /// External (host-specific) file attributes.
    pub external_attributes: u32,
    /// Per-entry compression method override; `None` falls back to `CompressOptions`.
    pub method: Option<CompressionMethod>,
    /// Per-entry deflate level override; `None` falls back to `CompressOptions`.
    pub level: Option<u32>,
}

impl ArchiveFile {
    /// Create a new archive file with default (unset) metadata.
    pub fn new(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        ArchiveFile {
            name: name.into(),
            data: data.into(),
            comment: None,
            extra_fields: Vec::new(),
            last_mod_time: 0,
            last_mod_date: 0,
            opsys: 3, // ZIP_OPSYS_UNIX
            external_attributes: 0,
            method: None,
            level: None,
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
    /// Encryption method to apply when writing via [`write_archive_encrypted`]
    /// (0 = none, 1 = traditional PKWARE). Ignored by [`write_archive`].
    pub encryption_method: u16,
}

impl Default for CompressOptions {
    fn default() -> Self {
        CompressOptions {
            method: CompressionMethod::Deflate,
            level: 6,
            parallel: true,
            workers: 0,
            large_file_threshold: 8 * 1024 * 1024,
            encryption_method: 0,
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
    /// Compressed byte length (includes the 12-byte encryption header when
    /// encrypted).
    pub comp_size: u64,
    /// Uncompressed byte length.
    pub uncomp_size: u64,
    /// Compressed bytes (for encrypted entries: the 12-byte header followed by
    /// the encrypted compressed bytes).
    pub data: Vec<u8>,
    /// Encryption method (0 = none, 1 = traditional PKWARE).
    pub encryption_method: u16,
    /// Entry comment (raw bytes), or `None`.
    pub comment: Option<Vec<u8>>,
    /// User extra fields `(id, data)`.
    pub extra_fields: Vec<(u16, Vec<u8>)>,
    /// DOS-encoded last modification time.
    pub last_mod_time: u16,
    /// DOS-encoded last modification date.
    pub last_mod_date: u16,
    /// Host system (upper byte of "version made by").
    pub opsys: u8,
    /// External (host-specific) file attributes.
    pub external_attributes: u32,
}

/// Compress a single byte slice. Deterministic given `(method, level, input)`.
pub fn compress_bytes(data: &[u8], method: CompressionMethod, level: u32) -> Result<Vec<u8>> {
    match method {
        CompressionMethod::Store => Ok(data.to_vec()),
        CompressionMethod::Deflate => {
            let lvl = flate2::Compression::new(level.min(9));
            // Random or already-compressed input can produce output close to
            // the input size. A bounded initial buffer avoids repeated Vec
            // growth/copy cycles without reserving the full input for highly
            // compressible text.
            let initial_capacity = data.len().min(256 * 1024);
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::with_capacity(initial_capacity), lvl);
            enc.write_all(data)
                .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))?;
            enc.finish()
                .map_err(|e| ZipError::with_system(ZipErrorCode::Write, e))
        }
        _ => Err(ZipError::new(ZipErrorCode::Compnotsupp)),
    }
}

/// Compress one archive file.
///
/// The effective compression method/level is the per-entry override (if set)
/// falling back to `CompressOptions`. This is what makes `zip_set_file_compression`
/// able to select Store/Deflate per entry while keeping the global default for
/// everything else.
pub fn compress_file(file: &ArchiveFile, opts: &CompressOptions) -> Result<CompressedFile> {
    let method = file.method.unwrap_or(opts.method);
    let level = file.level.unwrap_or(opts.level);
    let data = compress_bytes(&file.data, method, level)?;
    Ok(CompressedFile {
        name: file.name.clone(),
        method: method_to_u16(method),
        crc: crc32fast::hash(&file.data),
        comp_size: data.len() as u64,
        uncomp_size: file.data.len() as u64,
        data,
        encryption_method: 0,
        comment: file.comment.clone(),
        extra_fields: file.extra_fields.clone(),
        last_mod_time: file.last_mod_time,
        last_mod_date: file.last_mod_date,
        opsys: file.opsys,
        external_attributes: file.external_attributes,
    })
}

/// A cooperative progress/cancel polling callback for the write path.
///
/// The closure is invoked at cooperative checkpoints with the number of bytes
/// completed so far and the total bytes to process. It returns `true` to
/// request that the enclosing write be aborted with `ZIP_ER_CANCELLED` (no
/// output is committed); returning `false` continues normally. When no poll is
/// supplied the write behaves exactly as before (deterministic output, no
/// behaviour change to bytes).
pub type WriteProgressPoll<'a> = &'a mut dyn FnMut(u64, u64) -> bool;

/// Internal progress tracker threaded through the write path.
///
/// Tracks `completed`/`total` and invokes the user poll at each checkpoint,
/// mapping a cancel request to [`ZipErrorCode::Cancelled`].
struct WriteProgress<'a> {
    total: u64,
    completed: u64,
    poll: Option<WriteProgressPoll<'a>>,
}

impl<'a> WriteProgress<'a> {
    fn new(total: u64, poll: Option<WriteProgressPoll<'a>>) -> Self {
        WriteProgress {
            total,
            completed: 0,
            poll,
        }
    }

    /// Invoke the poll at the current `completed`/`total` position. Returns
    /// `Cancelled` if the poll requests cancellation.
    fn report(&mut self) -> Result<()> {
        if let Some(p) = self.poll.as_mut() {
            if p(self.completed, self.total) {
                return Err(ZipError::new(ZipErrorCode::Cancelled));
            }
        }
        Ok(())
    }

    /// Advance `completed` by `n` (clamped to `total`) and report.
    fn advance(&mut self, by: u64) -> Result<()> {
        self.completed = self.completed.saturating_add(by).min(self.total);
        self.report()
    }
}

/// Compress a batch of files, optionally in parallel, returning results in
/// **input order** (byte-identical to serial mode).
pub fn compress_files(
    files: &[ArchiveFile],
    opts: &CompressOptions,
) -> Result<Vec<CompressedFile>> {
    compress_files_hooked(files, opts, None)
}

/// Compress a batch of files, reporting progress / checking cancellation via
/// `progress` at each cooperative checkpoint.
///
/// When a progress hook is present the batch is compressed serially (progress
/// polling is inherently serial and the user callback may not be thread-safe),
/// still producing byte-identical output. When no hook is present this is
/// exactly [`compress_files`] (including the parallel path).
fn compress_files_hooked(
    files: &[ArchiveFile],
    opts: &CompressOptions,
    mut progress: Option<&mut WriteProgress>,
) -> Result<Vec<CompressedFile>> {
    if progress.is_some() {
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            if let Some(p) = progress.as_deref_mut() {
                p.advance(f.data.len() as u64)?;
            }
            out.push(compress_file(f, opts)?);
        }
        Ok(out)
    } else {
        compress_files_parallel_or_serial(files, opts)
    }
}

/// The original parallel/serial dispatch (extracted so the hooked path can
/// force serial while keeping the deterministic parallel behaviour).
fn compress_files_parallel_or_serial(
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
// deterministic writer with ZIP64 and encryption support.
// ---------------------------------------------------------------------------

/// Write a complete ZIP archive containing `files`, compressed per `opts`.
///
/// The layout is local-file-header + data for each member (in order), then the
/// central directory, then the EOCD record. The produced bytes can be re-read
/// with [`crate::Archive::open`]. Entries are written unencrypted.
pub fn write_archive(files: &[ArchiveFile], opts: &CompressOptions) -> Result<Vec<u8>> {
    let compressed = compress_files(files, opts)?;
    write_compressed(&compressed, &[])
}

/// Write a complete ZIP archive, encrypting the entries flagged in `encrypt`
/// (index-aligned with `files`) with traditional PKWARE (ZipCrypto) using
/// `password`.
///
/// `opts.encryption_method` is ignored here; the per-entry `encrypt` flags
/// decide which entries are encrypted. Encrypted entries get the `ENCRYPTED`
/// bit flag and a 12-byte encryption header prepended to their compressed data.
pub fn write_archive_encrypted(
    files: &[ArchiveFile],
    opts: &CompressOptions,
    password: &[u8],
    encrypt: &[bool],
) -> Result<Vec<u8>> {
    let methods: Vec<u16> = encrypt
        .iter()
        .map(|&e| {
            if e {
                crate::constant::encryption::TRAD_PKWARE
            } else {
                0
            }
        })
        .collect();
    write_archive_encrypted_methods(files, opts, password, &methods)
}

/// Write a complete ZIP archive, encrypting each entry with the per-entry
/// encryption `method` (`0` = none, `1` = traditional PKWARE/ZipCrypto, or one
/// of `ZIP_EM_AES_128/192/256`) using `password`.
///
/// AES entries are written as WinZip AES AE-2: on-disk compression method 99,
/// an `0x9901` extra field carrying the actual method + strength, the
/// `ENCRYPTED | DATA_DESCRIPTOR` bit flags, a CRC field of 0 (not stored for
/// AE-2), and a data region of `[salt][2-byte verify][AES-CTR ciphertext]
/// [10-byte HMAC-SHA1]`.
pub fn write_archive_encrypted_methods(
    files: &[ArchiveFile],
    opts: &CompressOptions,
    password: &[u8],
    methods: &[u16],
) -> Result<Vec<u8>> {
    let mut compressed = compress_files(files, opts)?;
    if methods.len() != compressed.len() {
        return Err(ZipError::new(ZipErrorCode::Inval));
    }
    for (cf, &em) in compressed.iter_mut().zip(methods) {
        if em == 0 {
            continue;
        }
        if em == crate::constant::encryption::TRAD_PKWARE {
            let encrypted = crate::crypto::encrypt_data(password, cf.crc, &cf.data);
            cf.data = encrypted;
            cf.comp_size = cf.data.len() as u64;
            cf.encryption_method = crate::constant::encryption::TRAD_PKWARE;
        } else if crate::crypto::is_aes_method(em) {
            let salt = crate::crypto::random_salt(crate::crypto::aes_salt_length(em));
            let encrypted = crate::crypto::aes_encrypt_data(password, em, &cf.data, &salt);
            cf.data = encrypted;
            cf.comp_size = cf.data.len() as u64;
            cf.encryption_method = em;
        } else {
            return Err(ZipError::new(ZipErrorCode::Encrmethnotsupp));
        }
    }
    write_compressed(&compressed, &[])
}

/// Write a complete ZIP archive carrying per-entry metadata plus an archive
/// (EOCD) comment, optionally encrypting each entry with a per-entry method.
///
/// This is the full write-path entry point used by the C ABI layer's
/// `materialize`: it compresses (with per-entry method/level overrides), applies
/// encryption, and emits local headers + data + central directory + EOCD with
/// the archive comment. Pass an empty `methods` slice for an unencrypted
/// archive and/or an empty `archive_comment` for no archive comment.
pub fn write_archive_full(
    files: &[ArchiveFile],
    opts: &CompressOptions,
    password: &[u8],
    methods: &[u16],
    archive_comment: &[u8],
) -> Result<Vec<u8>> {
    write_archive_full_with_progress(files, opts, password, methods, archive_comment, None)
}

/// Write a complete ZIP archive carrying per-entry metadata plus an archive
/// (EOCD) comment, optionally encrypting each entry with a per-entry method,
/// reporting progress / checking cancellation via `poll` at cooperative
/// checkpoints.
///
/// This is the full write-path entry point used by the C ABI layer's
/// `materialize`: it compresses (with per-entry method/level overrides), applies
/// encryption, and emits local headers + data + central directory + EOCD with
/// the archive comment. Pass an empty `methods` slice for an unencrypted
/// archive and/or an empty `archive_comment` for no archive comment.
///
/// The poll reports `(bytes_completed, bytes_total)` monotonically, reaching
/// `(total, total)` (progress 1.0) when the archive is fully written. If the
/// poll returns `true` the write is aborted with `ZIP_ER_CANCELLED` and no
/// output bytes are produced. When `poll` is `None` the output is byte-identical
/// to [`write_archive_full`].
pub fn write_archive_full_with_progress<'a>(
    files: &[ArchiveFile],
    opts: &CompressOptions,
    password: &[u8],
    methods: &[u16],
    archive_comment: &[u8],
    mut poll: Option<WriteProgressPoll<'a>>,
) -> Result<Vec<u8>> {
    let total: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    let mut progress = WriteProgress::new(total, poll.take());
    progress.report()?;

    let mut compressed = compress_files_hooked(files, opts, Some(&mut progress))?;
    if !methods.is_empty() {
        if methods.len() != compressed.len() {
            return Err(ZipError::new(ZipErrorCode::Inval));
        }
        for (cf, &em) in compressed.iter_mut().zip(methods) {
            if em == 0 {
                continue;
            }
            if em == crate::constant::encryption::TRAD_PKWARE {
                let encrypted = crate::crypto::encrypt_data(password, cf.crc, &cf.data);
                cf.data = encrypted;
                cf.comp_size = cf.data.len() as u64;
                cf.encryption_method = crate::constant::encryption::TRAD_PKWARE;
            } else if crate::crypto::is_aes_method(em) {
                let salt = crate::crypto::random_salt(crate::crypto::aes_salt_length(em));
                let encrypted = crate::crypto::aes_encrypt_data(password, em, &cf.data, &salt);
                cf.data = encrypted;
                cf.comp_size = cf.data.len() as u64;
                cf.encryption_method = em;
            } else {
                return Err(ZipError::new(ZipErrorCode::Encrmethnotsupp));
            }
        }
    }
    let out = write_compressed_hooked(&compressed, archive_comment, Some(&mut progress))?;
    progress.report()?; // final checkpoint at 100%
    Ok(out)
}

fn write_compressed(compressed: &[CompressedFile], archive_comment: &[u8]) -> Result<Vec<u8>> {
    write_compressed_hooked(compressed, archive_comment, None)
}

fn write_compressed_hooked(
    compressed: &[CompressedFile],
    archive_comment: &[u8],
    mut progress: Option<&mut WriteProgress>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cdir = Vec::new();
    let mut local_offsets = Vec::with_capacity(compressed.len());

    for cf in compressed {
        if let Some(p) = progress.as_deref_mut() {
            p.report()?;
        }
        let offset = out.len() as u64;
        local_offsets.push(offset);
        let entry_zip64 = needs_zip64_sizes(cf);
        write_local_header(cf, &mut out, entry_zip64)?;
        out.extend_from_slice(&cf.data);
        if crate::crypto::is_aes_method(cf.encryption_method) {
            write_data_descriptor(cf, entry_zip64, &mut out);
        }
        write_central_entry(cf, offset, &mut cdir)?;
    }

    let cdir_offset = out.len() as u64;
    out.extend_from_slice(&cdir);
    let cdir_size = cdir.len() as u64;
    let need_zip64 = compressed.len() > u16::MAX as usize
        || cdir_size > u32::MAX as u64
        || cdir_offset > u32::MAX as u64
        || compressed
            .iter()
            .zip(local_offsets.iter())
            .any(|(cf, &offset)| needs_zip64_sizes(cf) || offset > u32::MAX as u64);
    if need_zip64 {
        let zip64_eocd_offset = cdir_offset + cdir_size;
        crate::cdir::write_eocd64(compressed.len() as u64, cdir_size, cdir_offset, &mut out);
        crate::cdir::write_eocd64_locator(zip64_eocd_offset, &mut out);
        crate::cdir::write_eocd_zip64_sentinel(compressed.len() as u64, archive_comment, &mut out);
    } else {
        write_eocd(
            compressed.len() as u16,
            cdir_size,
            cdir_offset,
            archive_comment,
            &mut out,
        );
    }
    Ok(out)
}

fn needs_zip64_sizes(cf: &CompressedFile) -> bool {
    cf.comp_size > u32::MAX as u64 || cf.uncomp_size > u32::MAX as u64
}

fn write_local_header(cf: &CompressedFile, out: &mut Vec<u8>, entry_zip64: bool) -> Result<()> {
    let aes = crate::crypto::is_aes_method(cf.encryption_method);
    let flags = if cf.encryption_method != 0 {
        if aes {
            flag::ENCRYPTED | flag::DATA_DESCRIPTOR
        } else {
            flag::ENCRYPTED
        }
    } else {
        0
    };
    let method = on_disk_method(cf);
    let crc = if aes { 0 } else { cf.crc };
    let version_needed = if aes {
        51u16
    } else if entry_zip64 {
        45u16
    } else {
        20u16
    };
    let extra = build_extra(cf, entry_zip64, false, 0)?;
    let extra_len =
        u16::try_from(extra.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
    out.extend_from_slice(&magic::LOCAL);
    out.extend_from_slice(&version_needed.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes()); // bit flags
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&cf.last_mod_time.to_le_bytes()); // dos time
    out.extend_from_slice(&cf.last_mod_date.to_le_bytes()); // dos date
    out.extend_from_slice(&crc.to_le_bytes());
    let comp_size = if entry_zip64 {
        u32::MAX
    } else {
        cf.comp_size as u32
    };
    let uncomp_size = if entry_zip64 {
        u32::MAX
    } else {
        cf.uncomp_size as u32
    };
    out.extend_from_slice(&comp_size.to_le_bytes());
    out.extend_from_slice(&uncomp_size.to_le_bytes());
    out.extend_from_slice(&(cf.name.len() as u16).to_le_bytes());
    out.extend_from_slice(&extra_len.to_le_bytes()); // extra len
    out.extend_from_slice(cf.name.as_bytes());
    out.extend_from_slice(&extra);
    Ok(())
}

fn write_central_entry(cf: &CompressedFile, offset: u64, cdir: &mut Vec<u8>) -> Result<()> {
    let aes = crate::crypto::is_aes_method(cf.encryption_method);
    let flags = if cf.encryption_method != 0 {
        if aes {
            flag::ENCRYPTED | flag::DATA_DESCRIPTOR
        } else {
            flag::ENCRYPTED
        }
    } else {
        0
    };
    let method = on_disk_method(cf);
    let crc = if aes { 0 } else { cf.crc };
    let entry_zip64 = needs_zip64_sizes(cf) || offset > u32::MAX as u64;
    let version_needed = if aes {
        51u16
    } else if entry_zip64 {
        45u16
    } else {
        20u16
    };
    let extra = build_extra(cf, entry_zip64, true, offset)?;
    let extra_len =
        u16::try_from(extra.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
    let comment = cf.comment.as_deref().unwrap_or(&[]);
    let comment_len =
        u16::try_from(comment.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
    cdir.extend_from_slice(&magic::CENTRAL);
    cdir.extend_from_slice(&(((cf.opsys as u16) << 8) | version_needed).to_le_bytes()); // version made by: host system
    cdir.extend_from_slice(&version_needed.to_le_bytes()); // version needed
    cdir.extend_from_slice(&flags.to_le_bytes()); // bit flags
    cdir.extend_from_slice(&method.to_le_bytes());
    cdir.extend_from_slice(&cf.last_mod_time.to_le_bytes()); // dos time
    cdir.extend_from_slice(&cf.last_mod_date.to_le_bytes()); // dos date
    cdir.extend_from_slice(&crc.to_le_bytes());
    let comp_size = if cf.comp_size > u32::MAX as u64 {
        u32::MAX
    } else {
        cf.comp_size as u32
    };
    let uncomp_size = if cf.uncomp_size > u32::MAX as u64 {
        u32::MAX
    } else {
        cf.uncomp_size as u32
    };
    let offset32 = if offset > u32::MAX as u64 {
        u32::MAX
    } else {
        offset as u32
    };
    cdir.extend_from_slice(&comp_size.to_le_bytes());
    cdir.extend_from_slice(&uncomp_size.to_le_bytes());
    cdir.extend_from_slice(&(cf.name.len() as u16).to_le_bytes());
    cdir.extend_from_slice(&extra_len.to_le_bytes()); // extra len
    cdir.extend_from_slice(&comment_len.to_le_bytes()); // comment len
    cdir.extend_from_slice(&0u16.to_le_bytes()); // disk number
    cdir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    cdir.extend_from_slice(&cf.external_attributes.to_le_bytes()); // external attrs
    cdir.extend_from_slice(&offset32.to_le_bytes());
    cdir.extend_from_slice(cf.name.as_bytes());
    cdir.extend_from_slice(&extra);
    cdir.extend_from_slice(comment);
    Ok(())
}

/// Build the raw extra-field block for an entry: user extra fields (excluding
/// the internal ZIP64/AES ones the writer manages itself) followed by the
/// WinZip AES `0x9901` block when the entry is AES-encrypted.
fn build_extra(
    cf: &CompressedFile,
    entry_zip64: bool,
    central: bool,
    offset: u64,
) -> Result<Vec<u8>> {
    let mut extra = Vec::new();
    for (id, data) in &cf.extra_fields {
        if *id == EF_WINZIP_AES || *id == EXTRA_FIELD_ZIP64 {
            continue;
        }
        let len = u16::try_from(data.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
        extra.extend_from_slice(&id.to_le_bytes());
        extra.extend_from_slice(&len.to_le_bytes());
        extra.extend_from_slice(data);
    }
    if crate::crypto::is_aes_method(cf.encryption_method) {
        if let Some(aes_ef) = aes_extra_field(cf) {
            extra.extend_from_slice(&aes_ef);
        }
    }
    if entry_zip64 {
        let need_uncomp = cf.uncomp_size > u32::MAX as u64;
        let need_comp = cf.comp_size > u32::MAX as u64;
        // Local headers carry only size values. Central records additionally
        // carry the local-header offset when it overflows its 32-bit field.
        let need_offset = central && offset > u32::MAX as u64;
        let mut data = Vec::new();
        if need_uncomp {
            data.extend_from_slice(&cf.uncomp_size.to_le_bytes());
        }
        if need_comp {
            data.extend_from_slice(&cf.comp_size.to_le_bytes());
        }
        if need_offset {
            data.extend_from_slice(&offset.to_le_bytes());
        }
        if !data.is_empty() {
            let len =
                u16::try_from(data.len()).map_err(|_| ZipError::new(ZipErrorCode::Eftoolarge))?;
            extra.extend_from_slice(&EXTRA_FIELD_ZIP64.to_le_bytes());
            extra.extend_from_slice(&len.to_le_bytes());
            extra.extend_from_slice(&data);
        }
    }
    Ok(extra)
}

fn write_data_descriptor(cf: &CompressedFile, zip64: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(&magic::DATA_DESCRIPTOR);
    out.extend_from_slice(&0u32.to_le_bytes());
    if zip64 {
        out.extend_from_slice(&cf.comp_size.to_le_bytes());
        out.extend_from_slice(&cf.uncomp_size.to_le_bytes());
    } else {
        out.extend_from_slice(&(cf.comp_size as u32).to_le_bytes());
        out.extend_from_slice(&(cf.uncomp_size as u32).to_le_bytes());
    }
}

/// The on-disk compression-method field: for WinZip AES entries this is the
/// special `ZIP_CM_WINZIP_AES` (99); otherwise the real method.
fn on_disk_method(cf: &CompressedFile) -> u16 {
    if crate::crypto::is_aes_method(cf.encryption_method) {
        ZIP_CM_WINZIP_AES
    } else {
        cf.method
    }
}

/// Build the `0x9901` WinZip AES extra-field block (id + length + data) for an
/// AES entry, or `None` for a non-AES entry. Data: `u16` version (AE-2 = 2),
/// "AE" vendor, `u8` strength, `u16` actual compression method.
fn aes_extra_field(cf: &CompressedFile) -> Option<Vec<u8>> {
    if !crate::crypto::is_aes_method(cf.encryption_method) {
        return None;
    }
    let mut ef = Vec::with_capacity(4 + EF_WINZIP_AES_SIZE);
    ef.extend_from_slice(&EF_WINZIP_AES.to_le_bytes());
    ef.extend_from_slice(&(EF_WINZIP_AES_SIZE as u16).to_le_bytes());
    ef.extend_from_slice(&2u16.to_le_bytes()); // AE-2
    ef.extend_from_slice(b"AE");
    ef.push(crate::crypto::aes_strength(cf.encryption_method));
    ef.extend_from_slice(&cf.method.to_le_bytes());
    Some(ef)
}

fn write_eocd(
    num_entries: u16,
    cdir_size: u64,
    cdir_offset: u64,
    comment: &[u8],
    out: &mut Vec<u8>,
) {
    // Delegated to the shared serializer so both paths emit identical EOCDs.
    crate::cdir::write_eocd(num_entries, cdir_size, cdir_offset, comment, out);
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

    /// TC-1/TC-4 (in-process): progress polls fire monotonically non-decreasing
    /// and reach `(total, total)`; registering a non-cancelling poll does NOT
    /// change the produced bytes.
    #[test]
    fn write_progress_is_monotonic_and_reaches_total() {
        let files = sample_files();
        let opts = CompressOptions::default();

        // Baseline without any hook.
        let base = write_archive_full(&files, &opts, &[], &[], &[]).unwrap();

        let mut samples: Vec<(u64, u64)> = Vec::new();
        let mut poll = |completed: u64, total: u64| -> bool {
            samples.push((completed, total));
            false
        };
        let out = write_archive_full_with_progress(&files, &opts, &[], &[], &[], Some(&mut poll))
            .unwrap();

        assert!(
            samples.len() >= 2,
            "progress callback must fire at least twice (start + final)"
        );
        let mut last = 0u64;
        for (i, &(c, t)) in samples.iter().enumerate() {
            assert!(c >= last, "sample {i}: completed went backwards");
            assert!(c <= t, "sample {i}: completed > total");
            last = c;
        }
        let (final_c, final_t) = *samples.last().unwrap();
        assert_eq!(final_c, final_t, "final callback must reach total (1.0)");

        // Determinism gate (TC-4): registering a no-op hook changes nothing.
        assert_eq!(
            out, base,
            "registering a non-cancelling poll must not change bytes"
        );
    }

    /// TC-2 (in-process): a poll that requests cancellation aborts with
    /// `Cancelled` and no output is produced.
    #[test]
    fn write_progress_cancel_aborts() {
        let files = sample_files();
        let opts = CompressOptions::default();
        let mut calls = 0usize;
        // Cancel as soon as we are asked the first time.
        let mut poll = |_c: u64, _t: u64| -> bool {
            calls += 1;
            true
        };
        let err = write_archive_full_with_progress(&files, &opts, &[], &[], &[], Some(&mut poll))
            .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Cancelled);
        assert!(calls >= 1);
    }

    /// TC-5 (in-process): a poll over a zero-length archive must not panic and
    /// must still reach a defined final state.
    #[test]
    fn write_progress_zero_length_no_panic() {
        let opts = CompressOptions::default();
        let files: Vec<ArchiveFile> = Vec::new();
        let mut samples: Vec<(u64, u64)> = Vec::new();
        let mut poll = |c: u64, t: u64| -> bool {
            samples.push((c, t));
            false
        };
        let out = write_archive_full_with_progress(&files, &opts, &[], &[], &[], Some(&mut poll))
            .unwrap();
        assert!(!samples.is_empty());
        assert!(out.len() >= 22, "empty archive must still be a valid zip");
        let (fc, ft) = *samples.last().unwrap();
        assert_eq!(fc, ft, "zero-length: completed must equal total at end");
    }

    /// TC-5 (zip-core): malformed/edge-case progress inputs must never panic:
    /// a `None` poll, a poll that returns a garbage non-zero on every call, and
    /// a zero-length archive all yield a defined result.
    #[test]
    fn callbacks_malformed_no_panic() {
        let opts = CompressOptions::default();
        // None poll (no callbacks registered): plain success, no panic.
        let out =
            write_archive_full_with_progress(&sample_files(), &opts, &[], &[], &[], None).unwrap();
        assert!(out.len() > 22);

        // Poll that always requests cancellation (garbage non-zero): clean
        // Cancelled error, no panic.
        let mut poll = |_c: u64, _t: u64| -> bool { true };
        let err = write_archive_full_with_progress(
            &sample_files(),
            &opts,
            &[],
            &[],
            &[],
            Some(&mut poll),
        )
        .unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Cancelled);

        // Zero-length archive with a poll: no panic, valid empty zip.
        let empty: Vec<ArchiveFile> = Vec::new();
        let mut poll2 = |_c: u64, _t: u64| -> bool { false };
        let out2 = write_archive_full_with_progress(&empty, &opts, &[], &[], &[], Some(&mut poll2))
            .unwrap();
        assert!(out2.len() >= 22);
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

    #[test]
    fn zip64_entry_headers_preserve_overflow_values() {
        let cf = CompressedFile {
            name: "large.bin".to_string(),
            method: 0,
            crc: 0,
            comp_size: u32::MAX as u64 + 1,
            uncomp_size: u32::MAX as u64 + 2,
            data: Vec::new(),
            encryption_method: 0,
            comment: None,
            extra_fields: Vec::new(),
            last_mod_time: 0,
            last_mod_date: 0,
            opsys: 3,
            external_attributes: 0,
        };

        let mut local = Vec::new();
        write_local_header(&cf, &mut local, true).unwrap();
        let (local_entry, _) =
            crate::dirent::Dirent::parse_local(&mut Cursor::new(&local)).unwrap();
        assert_eq!(local_entry.comp_size, cf.comp_size);
        assert_eq!(local_entry.uncomp_size, cf.uncomp_size);

        let mut central = Vec::new();
        write_central_entry(&cf, u32::MAX as u64 + 3, &mut central).unwrap();
        let central_entry =
            crate::dirent::Dirent::parse_central(&mut Cursor::new(&central)).unwrap();
        assert_eq!(central_entry.comp_size, cf.comp_size);
        assert_eq!(central_entry.uncomp_size, cf.uncomp_size);
        assert_eq!(central_entry.offset, u32::MAX as u64 + 3);
    }

    #[test]
    fn zip64_entry_count_roundtrips() {
        let files: Vec<_> = (0..=u16::MAX)
            .map(|i| ArchiveFile::new(format!("{i}.txt"), Vec::new()))
            .collect();
        let opts = CompressOptions {
            method: CompressionMethod::Store,
            parallel: false,
            ..Default::default()
        };
        let bytes = write_archive(&files, &opts).unwrap();
        assert!(bytes.windows(4).any(|w| w == magic::EOCD64));
        assert!(bytes.windows(4).any(|w| w == magic::EOCD64_LOCATOR));
        let archive = Archive::open(Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), files.len() as u64);
    }

    #[test]
    fn aes_entries_emit_their_data_descriptor() {
        let files = vec![ArchiveFile::new("secret.txt", b"descriptor".repeat(32))];
        let bytes = write_archive_encrypted_methods(
            &files,
            &CompressOptions::default(),
            b"password",
            &[crate::constant::encryption::AES_256],
        )
        .unwrap();
        assert!(bytes.windows(4).any(|w| w == magic::DATA_DESCRIPTOR));
        let archive = Archive::open(Cursor::new(bytes)).unwrap();
        let mut reader = archive
            .open_entry_with_password(0, Some(b"password"))
            .unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, files[0].data);
    }
}

//! Benchmark crate for kzip.
//!
//! The criterion benchmarks live in `benches/`; this library target exists so
//! the crate is a normal workspace member that `cargo test --workspace` can
//! build cleanly. It also hosts the shared, deterministic benchmark corpus used
//! by the C-vs-Rust serial comparison harnesses (`c_serial` / `rust_serial`
//! binaries).

use std::path::Path;

/// One member of a benchmark corpus.
#[derive(Debug, Clone)]
pub struct CorpusFile {
    /// Member name stored in the archive.
    pub name: String,
    /// Uncompressed content.
    pub data: Vec<u8>,
}

/// A tiny deterministic xorshift64 PRNG so both the C and Rust serial harnesses
/// derive the *same* corpus bytes from the same seed without any shared state.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Highly compressible, deterministic "log line" text of length `n`.
fn repetitive_text(seed: u64, n: usize) -> Vec<u8> {
    let line = format!(
        "[{seed}] libzip-in-rust benchmark payload line with repeating content to exercise the DEFLATE codec; line content repeated for compression.\n"
    );
    let line = line.as_bytes();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take = (n - out.len()).min(line.len());
        out.extend_from_slice(&line[..take]);
    }
    out
}

/// A block mixing compressible text with incompressible random bytes, sized
/// `n` bytes.
fn mixed_block(seed: u64, n: usize, rng: &mut XorShift) -> Vec<u8> {
    let line = format!("medium block {seed} compressible text payload ...\n");
    let line = line.as_bytes();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take = ((rng.next_u64() % 1024) as usize + 1).min(n - out.len());
        if take % 3 == 0 {
            for _ in 0..take {
                out.push((rng.next_u64() & 0xff) as u8);
            }
        } else {
            let mut c = 0usize;
            while c < take {
                out.push(line[c % line.len()]);
                c += 1;
            }
        }
    }
    out
}

/// Build the shared mixed benchmark corpus: small/medium/large, text + random.
///
/// This is the *same* corpus both the C libzip serial harness (`c_serial`) and
/// the Rust serial harness (`rust_serial`) compress, so throughput numbers are
/// directly comparable.
pub fn build_mixed_corpus() -> Vec<CorpusFile> {
    let mut rng = XorShift::new(0x9E37_79B9_7F4A_7C15);
    let mut files = Vec::new();

    // 40 small, compressible text files.
    for i in 0..40 {
        let n = 512 + (rng.next_u64() % 4096) as usize;
        files.push(CorpusFile {
            name: format!("small/f{i:03}.txt"),
            data: repetitive_text(i as u64, n),
        });
    }

    // 8 medium files mixing text and semi-random data.
    for i in 0..8 {
        let n = 262_144 + (rng.next_u64() % 1_500_000) as usize;
        files.push(CorpusFile {
            name: format!("medium/m{i:02}.dat"),
            data: mixed_block(i as u64, n, &mut rng),
        });
    }

    // 2 large, highly compressible text files.
    files.push(CorpusFile {
        name: "large/big.txt".into(),
        data: repetitive_text(999, 32 * 1024 * 1024),
    });
    files.push(CorpusFile {
        name: "large/log.txt".into(),
        data: repetitive_text(1000, 16 * 1024 * 1024),
    });

    files
}

/// Total uncompressed corpus size in bytes.
pub fn corpus_size_bytes(files: &[CorpusFile]) -> u64 {
    files.iter().map(|f| f.data.len() as u64).sum()
}

/// Median of a slice of `f64`s.
pub fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = xs.len() / 2;
    if xs.len() % 2 == 0 {
        (xs[mid - 1] + xs[mid]) / 2.0
    } else {
        xs[mid]
    }
}

/// Format a CSV row for the serial benchmark results.
pub fn csv_row(
    impl_: &str,
    version: &str,
    run: usize,
    bytes: u64,
    secs: f64,
    mibps: f64,
) -> String {
    format!("{impl_},{version},{run},{bytes},{secs:.6},{mibps:.3}\n")
}

// ---------------------------------------------------------------------------
// Phase 5 §9.3 benchmark harness: shared corpus builders, the C libzip FFI
// surface (CApi), and CSV / timing helpers. All corpus builders are
// deterministic so C and Rust harnesses consume byte-identical inputs.
// ---------------------------------------------------------------------------

/// Deterministic corpus of `count` small files (1 KiB ..= 64 KiB), used for the
/// "compress small files" per-file-overhead + scheduling workload.
pub fn build_small_corpus(count: usize) -> Vec<CorpusFile> {
    let mut rng = XorShift::new(0xA5A5_5A5A_1234_5678);
    (0..count)
        .map(|i| {
            let size = 1024 + (rng.next_u64() % (64 * 1024 - 1024)) as usize; // 1 KiB..=64 KiB
            CorpusFile {
                name: format!("small/f{i:05}.txt"),
                data: repetitive_text(i as u64, size),
            }
        })
        .collect()
}

/// Deterministic corpus of `count` medium files (128 KiB ..= 2 MiB), all below
/// zip-core's default `large_file_threshold` (8 MiB), so the whole batch is
/// eligible for zip-core's parallel path. Used for the mixed/parallel workload.
pub fn build_parallel_corpus(count: usize) -> Vec<CorpusFile> {
    let size_classes = [128usize, 256, 512, 1024, 2048];
    (0..count)
        .map(|i| {
            let n = size_classes[i % size_classes.len()] * 1024;
            CorpusFile {
                name: format!("par/f{i:03}.txt"),
                data: repetitive_text(i as u64, n),
            }
        })
        .collect()
}

/// Deterministic corpus of `count` tiny entries (512 B ..= 4 KiB), used for the
/// many-entry read / random-access / memory-peak workloads.
pub fn build_many_entry_corpus(count: usize) -> Vec<CorpusFile> {
    (0..count)
        .map(|i| {
            let n = (i % 8 + 1) * 512;
            CorpusFile {
                name: format!("entry/{i:05}.txt"),
                data: repetitive_text(i as u64, n),
            }
        })
        .collect()
}

/// A single highly-compressible payload of `mib` MiB (used for the large-file
/// single-stream workload).
pub fn build_large_payload(mib: usize) -> Vec<u8> {
    repetitive_text(0x4C41_5247, mib * 1024 * 1024)
}

/// Header line for a benchmark CSV. Optional RSS column when `with_rss`.
pub fn csv_header(with_rss: bool) -> String {
    if with_rss {
        "impl,version,workload,run,uncompressed_bytes,seconds,mibps,rss_bytes\n".to_string()
    } else {
        "impl,version,workload,run,uncompressed_bytes,seconds,mibps\n".to_string()
    }
}

/// One CSV data row. `rss` is optional (Some => include RSS column).
#[allow(clippy::too_many_arguments)]
pub fn csv_row_wl(
    impl_: &str,
    version: &str,
    workload: &str,
    run: usize,
    bytes: u64,
    secs: f64,
    mibps: f64,
    rss: Option<u64>,
) -> String {
    match rss {
        Some(r) => format!("{impl_},{version},{workload},{run},{bytes},{secs:.6},{mibps:.3},{r}\n"),
        None => format!("{impl_},{version},{workload},{run},{bytes},{secs:.6},{mibps:.3}\n"),
    }
}

/// The C libzip FFI surface needed by every C harness in this crate. Loaded via
/// `libloading` so the original `zip.dll` runs in-process (fair, same machine,
/// same process scheduling as the Rust side).
pub mod capi {
    use libloading::Library;
    use std::ffi::{c_char, c_void, CStr};
    use std::path::Path;

    pub type ZipHandle = c_void;
    pub type ZipSource = c_void;
    pub type ZipFile = c_void;

    pub const ZIP_CREATE: i32 = 1;
    pub const ZIP_RDONLY: i32 = 16;
    pub const ZIP_FL_OVERWRITE: u32 = 8192;
    pub const ZIP_CM_DEFLATE: i32 = 8;
    pub const DEFLATE_LEVEL: u32 = 6;

    /// Opaque handle to the loaded C libzip.
    pub struct CApi {
        _lib: Library,
        pub zip_open: unsafe extern "C" fn(*const c_char, i32, *mut i32) -> *mut ZipHandle,
        pub zip_open_from_source:
            unsafe extern "C" fn(*mut ZipSource, i32, *mut c_void) -> *mut ZipHandle,
        pub zip_close: unsafe extern "C" fn(*mut ZipHandle) -> i32,
        pub zip_get_num_entries: unsafe extern "C" fn(*mut ZipHandle, u32) -> i64,
        pub zip_get_name: unsafe extern "C" fn(*mut ZipHandle, u64, u32) -> *const c_char,
        pub zip_file_add:
            unsafe extern "C" fn(*mut ZipHandle, *const c_char, *mut ZipSource, u32) -> i64,
        pub zip_set_file_compression: unsafe extern "C" fn(*mut ZipHandle, u64, i32, u32) -> i32,
        pub zip_source_buffer_create:
            unsafe extern "C" fn(*const c_void, u64, i32, *mut c_void) -> *mut ZipSource,
        pub zip_fopen: unsafe extern "C" fn(*mut ZipHandle, *const c_char, u32) -> *mut ZipFile,
        pub zip_fopen_index: unsafe extern "C" fn(*mut ZipHandle, u64, u32) -> *mut ZipFile,
        pub zip_fread: unsafe extern "C" fn(*mut ZipFile, *mut c_void, u64) -> i64,
        pub zip_fclose: unsafe extern "C" fn(*mut ZipFile) -> i32,
        pub zip_delete: unsafe extern "C" fn(*mut ZipHandle, u64) -> i32,
        pub zip_rename: unsafe extern "C" fn(*mut ZipHandle, u64, *const c_char) -> i32,
        pub zip_libzip_version: unsafe extern "C" fn() -> *const c_char,
    }

    impl CApi {
        /// Load the library at `path` and resolve every symbol we use.
        ///
        /// # Safety
        /// Must only be called once per `CApi`; the returned handle owns the
        /// loaded library. Resolved function pointers are only valid while the
        /// returned `CApi` (and its `_lib`) is alive.
        pub unsafe fn load(path: &Path) -> Result<Self, String> {
            let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
            fn resolve<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
                unsafe { lib.get::<T>(name.as_bytes()) }
                    .map(|s| *s)
                    .map_err(|e| format!("symbol {name}: {e}"))
            }
            Ok(CApi {
                zip_open: resolve(&lib, "zip_open")?,
                zip_open_from_source: resolve(&lib, "zip_open_from_source")?,
                zip_close: resolve(&lib, "zip_close")?,
                zip_get_num_entries: resolve(&lib, "zip_get_num_entries")?,
                zip_get_name: resolve(&lib, "zip_get_name")?,
                zip_file_add: resolve(&lib, "zip_file_add")?,
                zip_set_file_compression: resolve(&lib, "zip_set_file_compression")?,
                zip_source_buffer_create: resolve(&lib, "zip_source_buffer_create")?,
                zip_fopen: resolve(&lib, "zip_fopen")?,
                zip_fopen_index: resolve(&lib, "zip_fopen_index")?,
                zip_fread: resolve(&lib, "zip_fread")?,
                zip_fclose: resolve(&lib, "zip_fclose")?,
                zip_delete: resolve(&lib, "zip_delete")?,
                zip_rename: resolve(&lib, "zip_rename")?,
                zip_libzip_version: resolve(&lib, "zip_libzip_version")?,
                _lib: lib,
            })
        }

        /// libzip version string (e.g. "1.11.4").
        pub fn version(&self) -> String {
            unsafe { CStr::from_ptr((self.zip_libzip_version)()) }
                .to_str()
                .unwrap_or("unknown")
                .to_string()
        }
    }
}

/// Compress `files` with C libzip (serial, DEFLATE level 6) into `out_path`.
/// Returns `(elapsed_secs, mibps)` for the whole corpus. `freep = 0` keeps the
/// caller-owned buffers alive for the duration of the call.
///
/// # Safety
/// `api` must reference a valid, loaded `CApi`.
pub unsafe fn c_compress_serial(
    api: &capi::CApi,
    out_path: &Path,
    files: &[CorpusFile],
) -> Result<(f64, f64), String> {
    use std::ffi::{c_void, CString};
    let total_bytes = corpus_size_bytes(files);
    let cpath = CString::new(out_path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let mut errp: i32 = 0;

    let start = std::time::Instant::now();

    let za = (api.zip_open)(cpath.as_ptr(), capi::ZIP_CREATE, &mut errp);
    if za.is_null() {
        return Err(format!("zip_open failed err={errp}"));
    }
    for f in files {
        let cn = CString::new(f.name.clone()).map_err(|e| e.to_string())?;
        let src = (api.zip_source_buffer_create)(
            f.data.as_ptr() as *const c_void,
            f.data.len() as u64,
            0, // freep = 0: caller keeps the buffer alive
            std::ptr::null_mut(),
        );
        if src.is_null() {
            (api.zip_close)(za);
            return Err("zip_source_buffer_create failed".into());
        }
        let idx = (api.zip_file_add)(za, cn.as_ptr(), src, capi::ZIP_FL_OVERWRITE);
        if idx < 0 {
            (api.zip_close)(za);
            return Err("zip_file_add failed".into());
        }
        (api.zip_set_file_compression)(za, idx as u64, capi::ZIP_CM_DEFLATE, capi::DEFLATE_LEVEL);
    }
    let rc = (api.zip_close)(za);
    if rc != 0 {
        return Err(format!("zip_close failed rc={rc}"));
    }

    let secs = start.elapsed().as_secs_f64();
    if secs <= 0.0 {
        return Err("elapsed time was zero".into());
    }
    Ok((secs, total_bytes as f64 / secs / (1024.0 * 1024.0)))
}

/// Write a benchmark CSV file to `results/<name>.csv`.
pub fn write_csv(name: &str, content: &str) -> Result<(), String> {
    std::fs::write(format!("results/{name}.csv"), content)
        .map_err(|e| format!("cannot write results/{name}.csv: {e}"))
}

/// Path to the C libzip DLL, from `$ZIP_DLL` or the default `libs/c/zip.dll`.
pub fn zip_dll_path() -> String {
    std::env::var("ZIP_DLL").unwrap_or_else(|_| "libs/c/zip.dll".to_string())
}

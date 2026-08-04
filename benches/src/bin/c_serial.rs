//! C libzip serial compression benchmark harness.
//!
//! Loads the original C libzip (`libs/c/zip.dll`, or `$ZIP_DLL`) via
//! `libloading`, compresses the shared mixed corpus with DEFLATE level 6 by
//! adding each file as a buffer source and closing an archive to a temp file,
//! and reports serial throughput (MiB/s) as CSV to `results/c-serial.csv`.
//!
//! Usage: cargo run -p libzip-benches --bin c_serial [path-to-zip.dll]

use libzip_benches::{build_mixed_corpus, corpus_size_bytes, csv_row, median, CorpusFile};
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::time::Instant;

// libzip flags/constants (from libzip/zip.h).
const ZIP_CREATE: i32 = 1;
const ZIP_FL_OVERWRITE: u32 = 8192;
const ZIP_CM_DEFLATE: i32 = 8;
const DEFLATE_LEVEL: u32 = 6;
const ITERS: usize = 5;

type ZipHandle = c_void;
type ZipSource = c_void;

struct CApi {
    _lib: Library,
    zip_open: unsafe extern "C" fn(*const libc::c_char, i32, *mut i32) -> *mut ZipHandle,
    zip_file_add:
        unsafe extern "C" fn(*mut ZipHandle, *const libc::c_char, *mut ZipSource, u32) -> i64,
    zip_source_buffer_create:
        unsafe extern "C" fn(*const c_void, u64, i32, *mut c_void) -> *mut ZipSource,
    zip_set_file_compression: unsafe extern "C" fn(*mut ZipHandle, u64, i32, u32) -> i32,
    zip_close: unsafe extern "C" fn(*mut ZipHandle) -> i32,
    zip_libzip_version: unsafe extern "C" fn() -> *const libc::c_char,
}

use libloading::Library;

impl CApi {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
        fn resolve<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
            unsafe { lib.get::<T>(name.as_bytes()) }
                .map(|s| *s)
                .map_err(|e| format!("symbol {name}: {e}"))
        }
        Ok(CApi {
            zip_open: resolve(&lib, "zip_open")?,
            zip_file_add: resolve(&lib, "zip_file_add")?,
            zip_source_buffer_create: resolve(&lib, "zip_source_buffer_create")?,
            zip_set_file_compression: resolve(&lib, "zip_set_file_compression")?,
            zip_close: resolve(&lib, "zip_close")?,
            zip_libzip_version: resolve(&lib, "zip_libzip_version")?,
            _lib: lib,
        })
    }
}

/// Compress the whole corpus into `out_path`; returns throughput in MiB/s.
unsafe fn run_once(api: &CApi, out_path: &Path, files: &[CorpusFile]) -> Result<f64, String> {
    let total_bytes = corpus_size_bytes(files);
    let cpath = CString::new(out_path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let mut errp: i32 = 0;

    let start = Instant::now();

    let za = (api.zip_open)(cpath.as_ptr(), ZIP_CREATE, &mut errp);
    if za.is_null() {
        return Err(format!("zip_open failed err={errp}"));
    }
    for f in files {
        let cn = CString::new(f.name.clone()).map_err(|e| e.to_string())?;
        let src = (api.zip_source_buffer_create)(
            f.data.as_ptr() as *const c_void,
            f.data.len() as u64,
            0, // freep = 0: we keep the buffer alive
            std::ptr::null_mut(),
        );
        if src.is_null() {
            (api.zip_close)(za);
            return Err("zip_source_buffer_create failed".into());
        }
        let idx = (api.zip_file_add)(za, cn.as_ptr(), src, ZIP_FL_OVERWRITE);
        if idx < 0 {
            (api.zip_close)(za);
            return Err("zip_file_add failed".into());
        }
        (api.zip_set_file_compression)(za, idx as u64, ZIP_CM_DEFLATE, DEFLATE_LEVEL);
    }
    let rc = (api.zip_close)(za);
    if rc != 0 {
        return Err(format!("zip_close failed rc={rc}"));
    }

    let secs = start.elapsed().as_secs_f64();
    if secs <= 0.0 {
        return Err("elapsed time was zero".into());
    }
    Ok(total_bytes as f64 / secs / (1024.0 * 1024.0))
}

fn main() {
    let dll = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "libs/c/zip.dll".to_string());
    let api = unsafe { CApi::load(Path::new(&dll)) }.unwrap_or_else(|e| {
        eprintln!("failed to load library: {e}");
        std::process::exit(1);
    });
    let version = unsafe {
        CStr::from_ptr((api.zip_libzip_version)())
            .to_str()
            .unwrap_or("unknown")
            .to_string()
    };

    let files = build_mixed_corpus();
    let total = corpus_size_bytes(&files);
    let out_dir = std::env::temp_dir();
    let out_path: PathBuf = out_dir.join(format!("zipbench_c_{}.zip", std::process::id()));

    // Warmup.
    unsafe { run_once(&api, &out_path, &files) }.unwrap_or_else(|e| {
        eprintln!("warmup failed: {e}");
        std::process::exit(1);
    });

    let mut mibs = Vec::new();
    let mut secs_all = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let m = unsafe { run_once(&api, &out_path, &files) }.unwrap_or_else(|e| {
            eprintln!("run {run} failed: {e}");
            std::process::exit(1);
        });
        secs_all.push(t.elapsed().as_secs_f64());
        mibs.push(m);
    }
    let _ = std::fs::remove_file(&out_path);

    let mut csv = String::from("impl,version,run,uncompressed_bytes,seconds,mibps\n");
    for (run, (m, s)) in mibs.iter().zip(secs_all.iter()).enumerate() {
        csv.push_str(&csv_row("c_libzip", &version, run + 1, total, *s, *m));
    }
    std::fs::write("results/c-serial.csv", csv).unwrap_or_else(|e| {
        eprintln!("cannot write results/c-serial.csv: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "C libzip {} serial: corpus {} MiB, median {:.3} MiB/s over {} runs",
        version,
        total as f64 / (1024.0 * 1024.0),
        median(mibs),
        ITERS
    );
}

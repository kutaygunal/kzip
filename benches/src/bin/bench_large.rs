//! Benchmark: compress a LARGE single file (single-stream throughput) — Plan §9.3 workload 2.
//!
//! Compresses one ~1 GiB highly-compressible payload with C libzip (serial,
//! DEFLATE 6) vs `zip_core::compress_files` on a single file (which always
//! takes the serial path — no parallel win for one stream). Writes
//! `results/benchmark-large.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{
    build_large_payload, csv_header, csv_row_wl, median, write_csv, zip_dll_path,
};
use std::ffi::c_void;
use std::path::Path;
use std::time::Instant;
use zip_core::{compress_files, ArchiveFile, CompressOptions};

const ITERS: usize = 3;
const SIZE_MIB: usize = 1024; // 1 GiB

fn main() {
    let api = unsafe { CApi::load(Path::new(&zip_dll_path())) }
        .unwrap_or_else(|e| { eprintln!("load: {e}"); std::process::exit(1); });
    let cver = api.version();

    let payload = build_large_payload(SIZE_MIB);
    let total = payload.len() as u64;
    let archive = ArchiveFile::new("large/single.bin", payload.clone());
    let files = vec![archive];
    let out_dir = std::env::temp_dir();
    let out_path = out_dir.join(format!("bench_large_c_{}.zip", std::process::id()));

    // C serial single-stream compress.
    unsafe fn c_run(api: &CApi, out_path: &Path, payload: &[u8]) -> (f64, f64) {
        use std::ffi::CString;
        let cpath =
            CString::new(out_path.to_string_lossy().as_bytes()).expect("CString path");
        let mut errp: i32 = 0;
        let t = Instant::now();
        let za = (api.zip_open)(cpath.as_ptr(), libzip_benches::capi::ZIP_CREATE, &mut errp);
        assert!(!za.is_null(), "zip_open failed err={errp}");
        let name = CString::new("large/single.bin").unwrap();
        let src = (api.zip_source_buffer_create)(
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
            0,
            std::ptr::null_mut(),
        );
        assert!(!src.is_null());
        let idx = (api.zip_file_add)(za, name.as_ptr(), src, libzip_benches::capi::ZIP_FL_OVERWRITE);
        assert!(idx >= 0);
        let _ = (api.zip_set_file_compression)(
            za,
            idx as u64,
            libzip_benches::capi::ZIP_CM_DEFLATE,
            libzip_benches::capi::DEFLATE_LEVEL,
        );
        let rc = (api.zip_close)(za);
        assert_eq!(rc, 0, "zip_close failed");
        let secs = t.elapsed().as_secs_f64();
        (secs, payload.len() as f64 / secs / (1024.0 * 1024.0))
    }

    // Warmup.
    unsafe { c_run(&api, &out_path, &payload) };
    let _ = std::fs::remove_file(&out_path);

    let mut rows = String::new();
    rows.push_str(&csv_header(false));

    let mut c_mibs = Vec::new();
    for run in 0..ITERS {
        let _ = std::fs::remove_file(&out_path);
        let (secs, mibps) = unsafe { c_run(&api, &out_path, &payload) };
        c_mibs.push(mibps);
        rows.push_str(&csv_row_wl("c_libzip", &cver, "large", run + 1, total, secs, mibps, None));
    }
    let _ = std::fs::remove_file(&out_path);

    // Rust single-stream (serial path).
    let opts = CompressOptions {
        parallel: true, // even so, a single file never takes the parallel path
        workers: 0,
        ..Default::default()
    };
    let mut r_mibs = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let comp = compress_files(&files, &opts).expect("rust large compress failed");
        let secs = t.elapsed().as_secs_f64();
        let _sum: u64 = comp.iter().map(|c| c.comp_size).sum();
        r_mibs.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "large",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }

    write_csv("benchmark-large", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    eprintln!(
        "large-file ({} MiB): C {} median {:.1} MiB/s vs Rust {} median {:.1} MiB/s",
        SIZE_MIB,
        cver,
        median(c_mibs),
        env!("CARGO_PKG_VERSION"),
        median(r_mibs),
    );
}

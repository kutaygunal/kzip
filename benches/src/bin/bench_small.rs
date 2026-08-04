//! Benchmark: compress SMALL files (1 KiB ..= 64 KiB) — Plan §9.3 workload 1.
//!
//! Measures per-file overhead + scheduling for C libzip (serial) vs
//! `zip_core::compress_files` (serial), both DEFLATE level 6, on the identical
//! deterministic small-file corpus. Writes `results/benchmark-small.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{
    build_small_corpus, c_compress_serial, corpus_size_bytes, csv_header, csv_row_wl, median,
    write_csv, zip_dll_path,
};
use std::path::Path;
use std::time::Instant;
use zip_core::{compress_files, ArchiveFile, CompressOptions};

const ITERS: usize = 5;
const FILE_COUNT: usize = 3000;

/// Compress the corpus with zip-core in serial mode; returns (secs, mibps).
fn rust_serial(files: &[ArchiveFile]) -> (f64, f64) {
    let total = files.iter().map(|f| f.data.len() as u64).sum::<u64>();
    let opts = CompressOptions {
        parallel: false,
        workers: 0,
        ..Default::default()
    };
    let t = Instant::now();
    let comp = compress_files(files, &opts).expect("rust serial compress failed");
    let secs = t.elapsed().as_secs_f64();
    let _sum: u64 = comp.iter().map(|c| c.comp_size).sum();
    (secs, total as f64 / secs / (1024.0 * 1024.0))
}

fn main() {
    let api = unsafe { CApi::load(Path::new(&zip_dll_path())) }
        .unwrap_or_else(|e| { eprintln!("load: {e}"); std::process::exit(1); });
    let cver = api.version();

    let corpus = build_small_corpus(FILE_COUNT);
    let files: Vec<ArchiveFile> = corpus
        .iter()
        .map(|f| ArchiveFile::new(f.name.clone(), f.data.clone()))
        .collect();
    let total = corpus_size_bytes(&corpus);
    let out_dir = std::env::temp_dir();
    let out_path = out_dir.join(format!("bench_small_c_{}.zip", std::process::id()));

    // Warmup both sides.
    unsafe { c_compress_serial(&api, &out_path, &corpus) }.expect("C warmup");
    rust_serial(&files);

    let mut rows = String::new();
    rows.push_str(&csv_header(false));

    // C serial.
    let mut c_mibs = Vec::new();
    for run in 0..ITERS {
        let _ = std::fs::remove_file(&out_path);
        let (secs, mibps) =
            unsafe { c_compress_serial(&api, &out_path, &corpus) }.unwrap_or_else(|e| {
                eprintln!("C run {run}: {e}");
                std::process::exit(1);
            });
        c_mibs.push(mibps);
        rows.push_str(&csv_row_wl("c_libzip", &cver, "small", run + 1, total, secs, mibps, None));
    }
    let _ = std::fs::remove_file(&out_path);

    // Rust serial.
    let mut r_mibs = Vec::new();
    for run in 0..ITERS {
        let (secs, mibps) = rust_serial(&files);
        r_mibs.push(mibps);
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "small",
            run + 1,
            total,
            secs,
            mibps,
            None,
        ));
    }

    write_csv("benchmark-small", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    eprintln!(
        "small-files: C {} median {:.1} MiB/s vs Rust {} median {:.1} MiB/s ({} files, {} MiB)",
        cver,
        median(c_mibs),
        env!("CARGO_PKG_VERSION"),
        median(r_mibs),
        FILE_COUNT,
        total as f64 / (1024.0 * 1024.0),
    );
}

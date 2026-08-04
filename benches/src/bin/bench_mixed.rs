//! Benchmark for a MIXED corpus (many files, mixed sizes), exercising parallel
//! speedup and determinism (Plan §9.3 workload 3). Compares C libzip (serial,
//! single-threaded) against `zip-core` parallel (rayon, auto workers) and
//! asserts Rust parallel output is byte-identical to Rust serial. Writes
//! `results/benchmark-mixed.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{
    build_parallel_corpus, c_compress_serial, corpus_size_bytes, csv_header, csv_row_wl, median,
    write_csv, zip_dll_path,
};
use std::path::Path;
use std::time::Instant;
use zip_core::{compress_files, ArchiveFile, CompressOptions};

const ITERS: usize = 5;
const FILE_COUNT: usize = 96;

fn main() {
    let api = unsafe { CApi::load(Path::new(&zip_dll_path())) }.unwrap_or_else(|e| {
        eprintln!("load: {e}");
        std::process::exit(1);
    });
    let cver = api.version();

    let corpus = build_parallel_corpus(FILE_COUNT);
    let files: Vec<ArchiveFile> = corpus
        .iter()
        .map(|f| ArchiveFile::new(f.name.clone(), f.data.clone()))
        .collect();
    let total = corpus_size_bytes(&corpus);
    let out_dir = std::env::temp_dir();
    let out_path = out_dir.join(format!("bench_mixed_c_{}.zip", std::process::id()));

    // Determinism invariant (checked once): parallel must equal serial byte-for-byte.
    let serial_opts = CompressOptions {
        parallel: false,
        workers: 0,
        ..Default::default()
    };
    let par_opts = CompressOptions {
        parallel: true,
        workers: 0, // auto = available_parallelism
        ..Default::default()
    };
    let s = compress_files(&files, &serial_opts).unwrap();
    let p = compress_files(&files, &par_opts).unwrap();
    assert_eq!(s.len(), p.len(), "parallel changed entry count");
    for (a, b) in s.iter().zip(p.iter()) {
        assert_eq!(a.crc, b.crc, "crc diverged");
        assert_eq!(a.data, b.data, "parallel output diverged from serial");
    }

    // Warmup both.
    unsafe { c_compress_serial(&api, &out_path, &corpus) }.expect("C warmup");
    let _ = std::fs::remove_file(&out_path);
    let _ = compress_files(&files, &par_opts).unwrap();

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
        rows.push_str(&csv_row_wl(
            "c_libzip",
            &cver,
            "mixed_serial",
            run + 1,
            total,
            secs,
            mibps,
            None,
        ));
    }
    let _ = std::fs::remove_file(&out_path);

    // Rust parallel.
    let mut r_mibs = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let comp = compress_files(&files, &par_opts).expect("rust parallel compress failed");
        let secs = t.elapsed().as_secs_f64();
        let _sum: u64 = comp.iter().map(|c| c.comp_size).sum();
        r_mibs.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "mixed_parallel",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }

    // Rust serial on the same corpus, for an in-Rust parallel-vs-serial note.
    let mut s_mibs = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let comp = compress_files(&files, &serial_opts).unwrap();
        let secs = t.elapsed().as_secs_f64();
        let _sum: u64 = comp.iter().map(|c| c.comp_size).sum();
        s_mibs.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "mixed_serial",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }

    write_csv("benchmark-mixed", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let c_med = median(c_mibs);
    let s_med = median(s_mibs);
    let r_med = median(r_mibs);
    eprintln!(
        "mixed ({} files, {:.1} MiB): C serial {:.1} | Rust serial {:.1} | Rust parallel {:.1} MiB/s (speedup {:.2}x, deterministic: OK)",
        FILE_COUNT,
        total as f64 / (1024.0 * 1024.0),
        c_med,
        s_med,
        r_med,
        r_med / c_med,
    );
}

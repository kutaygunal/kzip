//! Rust zip-core serial compression benchmark harness.
//!
//! Compresses the shared mixed corpus with `zip_core::compress_files` in
//! serial mode (DEFLATE level 6, matching the C harness) and reports serial
//! throughput (MiB/s) as CSV to `results/rust-serial.csv`.
//!
//! Usage: cargo run -p libzip-benches --bin rust_serial

use libzip_benches::{build_mixed_corpus, corpus_size_bytes, csv_row, median};
use std::time::Instant;
use zip_core::{compress_files, ArchiveFile, CompressOptions};

const ITERS: usize = 5;

fn main() {
    let corpus = build_mixed_corpus();
    let files: Vec<ArchiveFile> = corpus
        .iter()
        .map(|f| ArchiveFile::new(f.name.clone(), f.data.clone()))
        .collect();
    let total = corpus_size_bytes(&corpus);
    let opts = CompressOptions {
        parallel: false,
        workers: 0,
        ..Default::default()
    };

    // Warmup.
    compress_files(&files, &opts).expect("warmup compression failed");

    let mut mibs = Vec::new();
    let mut secs_all = Vec::new();
    for _run in 0..ITERS {
        let t = Instant::now();
        let comp = compress_files(&files, &opts).expect("compression failed");
        let secs = t.elapsed().as_secs_f64();
        // Keep the compressed data alive so it isn't optimized away.
        let _sum: u64 = comp.iter().map(|c| c.comp_size).sum();
        secs_all.push(secs);
        mibs.push(total as f64 / secs / (1024.0 * 1024.0));
    }

    let mut csv = String::from("impl,version,run,uncompressed_bytes,seconds,mibps\n");
    for (run, (m, s)) in mibs.iter().zip(secs_all.iter()).enumerate() {
        csv.push_str(&csv_row("rust_zip_core", env!("CARGO_PKG_VERSION"), run + 1, total, *s, *m));
    }
    std::fs::write("results/rust-serial.csv", csv).unwrap_or_else(|e| {
        eprintln!("cannot write results/rust-serial.csv: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "Rust zip-core {} serial: corpus {} MiB, median {:.3} MiB/s over {} runs",
        env!("CARGO_PKG_VERSION"),
        total as f64 / (1024.0 * 1024.0),
        median(mibs),
        ITERS
    );
}

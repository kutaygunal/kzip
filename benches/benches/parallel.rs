//! Parallel compression scaling benchmark.
//!
//! Measures `zip_core::compress_files` for a multi-file corpus across a serial
//! baseline and 1/2/4/8 rayon workers, and asserts the deterministic
//! property that parallel output is byte-identical to serial.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use zip_core::{compress_files, ArchiveFile, CompressOptions};

/// Build a corpus of `count` independent, compressible files of varying sizes.
///
/// Files range from ~128 KiB to ~2 MiB so that DEFLATE work (not per-file
/// scheduling) dominates the runtime — giving a meaningful read on parallel
/// scaling. The determinism assertion below still requires byte-identical
/// output between serial and parallel.
fn build_corpus(count: usize) -> Vec<ArchiveFile> {
    let size_classes = [128usize, 256, 512, 1024, 2048];
    (0..count)
        .map(|i| {
            let k = size_classes[i % size_classes.len()]; // KiB
            let n = k * 1024;
            let content: Vec<u8> =
                format!("entry {i} with a compressible repeating payload of roughly {n} bytes ",)
                    .repeat(32)
                    .into_bytes();
            // Build exactly ~n bytes of compressible text.
            let mut data = Vec::with_capacity(n);
            while data.len() < n {
                data.extend_from_slice(&content);
            }
            data.truncate(n);
            ArchiveFile::new(format!("f{i:03}.txt"), data)
        })
        .collect()
}

fn bench_parallel(c: &mut Criterion) {
    let files = build_corpus(64);

    // Serial baseline.
    let serial_opts = CompressOptions {
        parallel: false,
        workers: 0,
        ..Default::default()
    };
    c.bench_function("compress_serial", |b| {
        b.iter(|| compress_files(&files, &serial_opts).unwrap());
    });

    // Parallel scaling across worker counts (each run uses its own bounded pool).
    let mut group = c.benchmark_group("compress_parallel");
    for workers in [1usize, 2, 4, 8] {
        let opts = CompressOptions {
            parallel: true,
            workers,
            ..Default::default()
        };
        group.bench_with_input(BenchmarkId::from_parameter(workers), &workers, |b, _| {
            b.iter(|| compress_files(&files, &opts).unwrap());
        });
    }
    group.finish();

    // Determinism invariant: parallel output must equal serial output
    // byte-for-byte (checked once when the benchmark harness starts).
    let serial = compress_files(&files, &serial_opts).unwrap();
    let par = compress_files(
        &files,
        &CompressOptions {
            parallel: true,
            workers: 4,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(serial.len(), par.len());
    for (s, p) in serial.iter().zip(par.iter()) {
        assert_eq!(s.crc, p.crc, "crc mismatch for {}", s.name);
        assert_eq!(
            s.data, p.data,
            "parallel output diverged from serial for {}",
            s.name
        );
    }
    // Single very-large file must fall back to serial and still work.
    let big = vec![vec![b'q'; 256 * 1024]];
    let big_files: Vec<ArchiveFile> = big
        .into_iter()
        .enumerate()
        .map(|(i, d)| ArchiveFile::new(format!("big{i}.bin"), d))
        .collect();
    let _ = compress_files(
        &big_files,
        &CompressOptions {
            parallel: true,
            large_file_threshold: 128 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
}

criterion_group!(benches, bench_parallel);
criterion_main!(benches);

//! Benchmark: read RANDOM entries (random-access / seek cost) — Plan §9.3 workload 5.
//!
//! Builds a 10k-entry archive on disk, then deterministically reads `SAMPLES`
//! random entries to EOF. Both sides read from the same file: C libzip via
//! `zip_open` + `zip_fopen_index`, Rust via `Archive::open(File)` +
//! `open_entry` (whose per-entry `duplicate()` is a cheap file-handle clone +
//! seek, not a buffer copy). This isolates the true seek / random-access cost.
//! Writes `results/benchmark-random.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{build_many_entry_corpus, csv_header, csv_row_wl, median, write_csv, zip_dll_path};
use std::ffi::{c_void, CString};
use std::io::Read;
use std::path::Path;
use std::time::Instant;
use zip_core::{write_archive, Archive, ArchiveFile, CompressOptions};

const ITERS: usize = 5;
const ENTRY_COUNT: usize = 10_000;
const SAMPLES: usize = 2000;

/// Deterministic xorshift64 index sequence.
fn sample_indices(seed: u64, n: usize, count: usize) -> Vec<u64> {
    let mut x = seed.max(1);
    (0..count)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n as u64
        })
        .collect()
}

fn main() {
    let api = unsafe { CApi::load(Path::new(&zip_dll_path())) }
        .unwrap_or_else(|e| { eprintln!("load: {e}"); std::process::exit(1); });
    let cver = api.version();

    let corpus = build_many_entry_corpus(ENTRY_COUNT);
    let files: Vec<ArchiveFile> = corpus
        .iter()
        .map(|f| ArchiveFile::new(f.name.clone(), f.data.clone()))
        .collect();
    let opts = CompressOptions {
        parallel: true,
        workers: 0,
        ..Default::default()
    };
    let bytes = write_archive(&files, &opts).expect("write_archive failed");
    let indices = sample_indices(0xC0FF_EE11_2233_4455, ENTRY_COUNT, SAMPLES);
    let total_read: u64 = indices.iter().map(|&i| files[i as usize].data.len() as u64).sum();
    let arch_path = std::env::temp_dir().join(format!("bench_random_{}.zip", std::process::id()));
    std::fs::write(&arch_path, &bytes).expect("write archive to disk");

    // ---- C random reads from file ----
    unsafe fn c_open(api: &CApi, path: &Path) -> *mut libzip_benches::capi::ZipHandle {
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut errp: i32 = 0;
        let za = (api.zip_open)(cpath.as_ptr(), 0, &mut errp);
        assert!(!za.is_null(), "zip_open failed err={errp}");
        za
    }
    unsafe fn c_sweep(api: &CApi, za: *mut libzip_benches::capi::ZipHandle, idx: &[u64]) -> (u64, f64) {
        let t = Instant::now();
        let mut total = 0u64;
        for &i in idx {
            let fh = (api.zip_fopen_index)(za, i, 0);
            if fh.is_null() {
                continue;
            }
            let mut buf = [0u8; 4096];
            loop {
                let r = (api.zip_fread)(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64);
                if r <= 0 {
                    break;
                }
                total += r as u64;
            }
            (api.zip_fclose)(fh);
        }
        let secs = t.elapsed().as_secs_f64();
        (total, secs)
    }

    // ---- Rust random reads from file ----
    let arch = Archive::open(std::fs::File::open(&arch_path).expect("open archive file"))
        .expect("rust open failed");
    fn rust_read_entry(arch: &Archive, idx: u64) -> u64 {
        let mut r = arch.open_entry(idx).expect("open_entry failed");
        let mut buf = [0u8; 4096];
        let mut total = 0u64;
        loop {
            let n = r.read(&mut buf).expect("read failed");
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    }

    // Warmup both.
    unsafe {
        let za = c_open(&api, &arch_path);
        c_sweep(&api, za, &indices);
        (api.zip_close)(za);
    }
    for &i in indices.iter().take(16) {
        let _ = rust_read_entry(&arch, i);
    }

    let mut rows = String::new();
    rows.push_str(&csv_header(false));

    let za = unsafe { c_open(&api, &arch_path) };
    let mut c_rates = Vec::new();
    for run in 0..ITERS {
        let (total, secs) = unsafe { c_sweep(&api, za, &indices) };
        assert_eq!(total, total_read, "C random read total mismatch");
        c_rates.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "c_libzip", &cver, "read_random", run + 1, total, secs,
            total as f64 / secs / (1024.0 * 1024.0), None,
        ));
    }
    unsafe { (api.zip_close)(za) };

    let mut r_rates = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let mut total = 0u64;
        for &i in &indices {
            total += rust_read_entry(&arch, i);
        }
        let secs = t.elapsed().as_secs_f64();
        assert_eq!(total, total_read, "Rust random read total mismatch");
        r_rates.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "rust_zip_core", env!("CARGO_PKG_VERSION"), "read_random", run + 1, total, secs,
            total as f64 / secs / (1024.0 * 1024.0), None,
        ));
    }

    write_csv("benchmark-random", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    std::fs::remove_file(&arch_path).ok();

    eprintln!(
        "read_random ({} samples of {} entries, {:.2} MiB, from file): C {:.1} vs Rust {:.1} MiB/s",
        SAMPLES, ENTRY_COUNT,
        total_read as f64 / (1024.0 * 1024.0),
        median(c_rates),
        median(r_rates),
    );
}

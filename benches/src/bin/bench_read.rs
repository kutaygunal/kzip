//! Benchmark: read / extract a FULL archive (decode throughput) — Plan §9.3 workload 4.
//!
//! Generates a deterministic archive on disk, then reads every entry to EOF.
//! Both sides read from the same on-disk archive: C libzip via `zip_open` +
//! `zip_fopen_index`, Rust via `Archive::open(File)` + `EntryReader`. This keeps
//! the comparison fair (no in-memory buffer-clone artifact; both stream-decode
//! from disk). Writes `results/benchmark-read.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{
    build_parallel_corpus, csv_header, csv_row_wl, median, write_csv, zip_dll_path,
};
use std::ffi::{c_void, CString};
use std::io::Read;
use std::path::Path;
use std::time::Instant;
use zip_core::{write_archive, Archive, ArchiveFile, CompressOptions};

const ITERS: usize = 5;
const FILE_COUNT: usize = 128;

fn main() {
    let api = unsafe { CApi::load(Path::new(&zip_dll_path())) }.unwrap_or_else(|e| {
        eprintln!("load: {e}");
        std::process::exit(1);
    });
    let cver = api.version();

    // Build the archive bytes once and write to disk.
    let corpus = build_parallel_corpus(FILE_COUNT);
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
    let total_uncomp: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    let arch_path = std::env::temp_dir().join(format!("bench_read_{}.zip", std::process::id()));
    std::fs::write(&arch_path, &bytes).expect("write archive to disk");

    // ---- C: open the on-disk archive once ----
    unsafe fn c_open(api: &CApi, path: &Path) -> *mut libzip_benches::capi::ZipHandle {
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut errp: i32 = 0;
        let za = (api.zip_open)(cpath.as_ptr(), 0, &mut errp);
        assert!(!za.is_null(), "zip_open failed err={errp}");
        za
    }
    unsafe fn c_sweep(api: &CApi, za: *mut libzip_benches::capi::ZipHandle) -> (u64, f64) {
        let n = (api.zip_get_num_entries)(za, 0);
        let t = Instant::now();
        let mut total = 0u64;
        for i in 0..n {
            let fh = (api.zip_fopen_index)(za, i as u64, 0);
            if fh.is_null() {
                continue;
            }
            let mut buf = [0u8; 65536];
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

    // ---- Rust: open the on-disk archive once ----
    let arch = Archive::open(std::fs::File::open(&arch_path).expect("open archive file"))
        .expect("rust open failed");

    // Warmup both.
    unsafe {
        let za = c_open(&api, &arch_path);
        c_sweep(&api, za);
        (api.zip_close)(za);
    }
    {
        let mut r = arch.open_entry(0).unwrap();
        let mut sink = Vec::new();
        let _ = r.read_to_end(&mut sink);
    }

    let mut rows = String::new();
    rows.push_str(&csv_header(false));

    // C sweep (keep archive open across runs).
    let za = unsafe { c_open(&api, &arch_path) };
    let mut c_mibs = Vec::new();
    for run in 0..ITERS {
        let (total, secs) = unsafe { c_sweep(&api, za) };
        assert_eq!(total, total_uncomp, "C read total mismatch");
        c_mibs.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "c_libzip",
            &cver,
            "read_full",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }
    unsafe { (api.zip_close)(za) };

    // Rust sweep.
    let mut r_mibs = Vec::new();
    for run in 0..ITERS {
        let t = Instant::now();
        let mut total = 0u64;
        for i in 0..arch.len() {
            let mut r = arch.open_entry(i).expect("open_entry failed");
            let mut buf = [0u8; 65536];
            loop {
                let n = r.read(&mut buf).expect("read failed");
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
        }
        let secs = t.elapsed().as_secs_f64();
        assert_eq!(total, total_uncomp, "Rust read total mismatch");
        r_mibs.push(total as f64 / secs / (1024.0 * 1024.0));
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "read_full",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }

    write_csv("benchmark-read", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    std::fs::remove_file(&arch_path).ok();

    eprintln!(
        "read_full ({} entries, {:.1} MiB, from file): C {:.1} vs Rust {:.1} MiB/s",
        FILE_COUNT,
        total_uncomp as f64 / (1024.0 * 1024.0),
        median(c_mibs),
        median(r_mibs),
    );
}

//! Benchmark: memory peak on a 10k-entry archive — Plan §9.3 workload 7.
//!
//! Rust side: a counting global allocator reports the high-water-mark live
//! bytes during `write_archive` (compress) and during `Archive::open` + read-all
//! (decode) of a 10k-entry archive. C side: approximate RSS delta via
//! `GetProcessMemoryInfo` (working-set) around the same operations. The C RSS
//! reading is an approximation because a process shared allocator may reuse
//! freed memory (noted as a limitation). Writes `results/benchmark-memory.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{build_many_entry_corpus, csv_header, csv_row_wl, median, write_csv, zip_dll_path};
use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::c_void;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::time::Instant;
use zip_core::{write_archive, Archive, ArchiveFile, CompressOptions};

const ITERS: usize = 3;
const ENTRY_COUNT: usize = 10_000;

// ---- counting global allocator (high-water-mark live bytes) ----
struct CountingAlloc;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn bump(delta: isize) {
    let cur = LIVE.fetch_add(delta, Ordering::Relaxed) + delta;
    if cur > 0 {
        PEAK.fetch_max(cur as usize, Ordering::Relaxed);
    }
}

// SAFETY: forwards to `System` and only adds atomic accounting; the bench crate
// (not zip-core/zip-async) may use unsafe.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            bump(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            bump(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        bump(-(l.size() as isize));
        System.dealloc(p, l);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let np = System.realloc(p, l, n);
        if !np.is_null() {
            bump(n as isize - l.size() as isize);
        }
        np
    }
}
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

// ---- Windows RSS helpers (C side) ----
#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged_pool: usize,
    quota_paged_pool: usize,
    quota_peak_nonpaged_pool: usize,
    quota_nonpaged_pool: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(
        h: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
}

fn current_working_set() -> usize {
    unsafe {
        let mut pmc = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set: 0,
            working_set: 0,
            quota_peak_paged_pool: 0,
            quota_paged_pool: 0,
            quota_peak_nonpaged_pool: 0,
            quota_nonpaged_pool: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let h = GetCurrentProcess();
        GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        pmc.working_set
    }
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
    let total: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    let bytes = write_archive(&files, &opts).expect("write_archive failed");
    let out_path = std::env::temp_dir().join(format!("bench_mem_{}.zip", std::process::id()));

    // ---- Rust: compress memory ----
    let mut rows = String::new();
    rows.push_str(&csv_header(true));
    let mut r_compress_peak = Vec::new();
    for run in 0..ITERS {
        PEAK.store(0, Ordering::Relaxed);
        LIVE.store(0, Ordering::Relaxed);
        let t = Instant::now();
        let out = write_archive(&files, &opts).unwrap();
        let secs = t.elapsed().as_secs_f64();
        let peak = PEAK.load(Ordering::Relaxed);
        r_compress_peak.push(peak);
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "memory_compress",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            Some(peak as u64),
        ));
        let _ = out.len();
    }

    // ---- Rust: read memory (open + read all, from file, clean decode buffers) ----
    let read_path = std::env::temp_dir().join(format!("bench_mem_read_{}.zip", std::process::id()));
    std::fs::write(&read_path, &bytes).expect("write archive for read");
    let arch = Archive::open(std::fs::File::open(&read_path).expect("open archive"))
        .expect("rust open failed");
    let mut r_read_peak = Vec::new();
    for run in 0..ITERS {
        PEAK.store(0, Ordering::Relaxed);
        LIVE.store(0, Ordering::Relaxed);
        let t = Instant::now();
        let mut read_total = 0u64;
        for i in 0..arch.len() {
            let mut r = arch.open_entry(i).unwrap();
            let mut buf = [0u8; 4096];
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                read_total += n as u64;
            }
        }
        let secs = t.elapsed().as_secs_f64();
        let peak = PEAK.load(Ordering::Relaxed);
        r_read_peak.push(peak);
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "memory_read",
            run + 1,
            read_total,
            secs,
            read_total as f64 / secs / (1024.0 * 1024.0),
            Some(peak as u64),
        ));
        let _ = arch.len();
    }
    std::fs::remove_file(&read_path).ok();

    // ---- C: compress memory (RSS delta) ----
    let mut c_compress_rss = Vec::new();
    for run in 0..ITERS {
        let _ = std::fs::remove_file(&out_path);
        let before = current_working_set();
        let t = Instant::now();
        unsafe {
            let cpath = std::ffi::CString::new(out_path.to_string_lossy().as_bytes()).unwrap();
            let mut errp: i32 = 0;
            let za = (api.zip_open)(cpath.as_ptr(), libzip_benches::capi::ZIP_CREATE, &mut errp);
            assert!(!za.is_null());
            for f in &corpus {
                let cn = std::ffi::CString::new(f.name.clone()).unwrap();
                let src = (api.zip_source_buffer_create)(
                    f.data.as_ptr() as *const c_void,
                    f.data.len() as u64,
                    0,
                    std::ptr::null_mut(),
                );
                let idx = (api.zip_file_add)(za, cn.as_ptr(), src, libzip_benches::capi::ZIP_FL_OVERWRITE);
                let _ = (api.zip_set_file_compression)(
                    za,
                    idx as u64,
                    libzip_benches::capi::ZIP_CM_DEFLATE,
                    libzip_benches::capi::DEFLATE_LEVEL,
                );
            }
            let rc = (api.zip_close)(za);
            assert_eq!(rc, 0);
        }
        let secs = t.elapsed().as_secs_f64();
        let after = current_working_set();
        let rss = after.saturating_sub(before);
        c_compress_rss.push(rss);
        rows.push_str(&csv_row_wl(
            "c_libzip",
            &cver,
            "memory_compress",
            run + 1,
            total,
            secs,
            total as f64 / secs / (1024.0 * 1024.0),
            Some(rss as u64),
        ));
    }
    let _ = std::fs::remove_file(&out_path);

    // ---- C: read memory (RSS delta) ----
    let mut c_read_rss = Vec::new();
    for run in 0..ITERS {
        let before = current_working_set();
        let t = Instant::now();
        let read_total = unsafe {
            let src = (api.zip_source_buffer_create)(
                bytes.as_ptr() as *const c_void,
                bytes.len() as u64,
                0,
                std::ptr::null_mut(),
            );
            let za = (api.zip_open_from_source)(
                src,
                libzip_benches::capi::ZIP_RDONLY,
                std::ptr::null_mut(),
            );
            assert!(!za.is_null());
            let n = (api.zip_get_num_entries)(za, 0);
            let mut rt = 0u64;
            for i in 0..n {
                let fh = (api.zip_fopen_index)(za, i as u64, 0);
                let mut buf = [0u8; 4096];
                loop {
                    let r = (api.zip_fread)(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64);
                    if r <= 0 {
                        break;
                    }
                    rt += r as u64;
                }
                (api.zip_fclose)(fh);
            }
            (api.zip_close)(za);
            rt
        };
        let secs = t.elapsed().as_secs_f64();
        let after = current_working_set();
        let rss = after.saturating_sub(before);
        c_read_rss.push(rss);
        rows.push_str(&csv_row_wl(
            "c_libzip",
            &cver,
            "memory_read",
            run + 1,
            read_total,
            secs,
            read_total as f64 / secs / (1024.0 * 1024.0),
            Some(rss as u64),
        ));
    }

    write_csv("benchmark-memory", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    eprintln!(
        "memory ({} entries, {:.1} MiB): Rust compress peak {:.1} MiB / read peak {:.1} MiB; C RSS delta compress {:.1} MiB / read {:.1} MiB",
        ENTRY_COUNT,
        total as f64 / (1024.0 * 1024.0),
        median(r_compress_peak.iter().map(|&x| x as f64).collect()) / (1024.0 * 1024.0),
        median(r_read_peak.iter().map(|&x| x as f64).collect()) / (1024.0 * 1024.0),
        median(c_compress_rss.iter().map(|&x| x as f64).collect()) / (1024.0 * 1024.0),
        median(c_read_rss.iter().map(|&x| x as f64).collect()) / (1024.0 * 1024.0),
    );
}

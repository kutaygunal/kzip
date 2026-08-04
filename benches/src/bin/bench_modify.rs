//! Benchmark: modify in place (add/delete/rename) — Plan §9.3 workload 6.
//!
//! C libzip supports true in-place modification (`zip_open` + `zip_delete` +
//! `zip_rename` + `zip_close`), which reuses already-compressed member data and
//! only rewrites the central directory. Rust `zip-core` has no in-place edit
//! API, so we build the Rust side as a rewrite model (read source corpus +
//! `write_archive`, recompressing every member). This is the documented
//! apples-to-oranges comparison: the Rust rewrite does strictly more work.
//! Writes `results/benchmark-modify.csv`.

use libzip_benches::capi::CApi;
use libzip_benches::{
    build_parallel_corpus, csv_header, csv_row_wl, median, write_csv, zip_dll_path,
};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::path::Path;
use std::time::Instant;
use zip_core::{write_archive, ArchiveFile, CompressOptions};

const ITERS: usize = 5;
const FILE_COUNT: usize = 64;

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
    let opts = CompressOptions {
        parallel: true,
        workers: 0,
        ..Default::default()
    };
    let total_uncomp: u64 = files.iter().map(|f| f.data.len() as u64).sum();

    // Modification plan (on ORIGINAL indices).
    let renames: Vec<(u64, &str)> = vec![
        (5, "renamed/f05.txt"),
        (15, "renamed/f15.txt"),
        (25, "renamed/f25.txt"),
    ];
    let deletes: Vec<u64> = vec![50, 40, 30, 20, 10]; // descending order

    // Build the original archive on disk for the C side.
    let original_bytes = write_archive(&files, &opts).expect("write_archive failed");
    let arch_path = std::env::temp_dir().join(format!("bench_modify_{}.zip", std::process::id()));
    std::fs::write(&arch_path, &original_bytes).expect("write original archive");

    // ---- C in-place modify ----
    unsafe fn c_modify(api: &CApi, path: &Path, renames: &[(u64, &str)], deletes: &[u64]) -> f64 {
        let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut errp: i32 = 0;
        let t = Instant::now();
        let za = (api.zip_open)(cpath.as_ptr(), libzip_benches::capi::ZIP_CREATE, &mut errp);
        assert!(!za.is_null(), "zip_open failed err={errp}");
        for &(idx, name) in renames {
            let cn = CString::new(name).unwrap();
            let rc = (api.zip_rename)(za, idx, cn.as_ptr());
            assert_eq!(rc, 0, "zip_rename idx={idx} failed");
        }
        for &d in deletes {
            let rc = (api.zip_delete)(za, d);
            assert_eq!(rc, 0, "zip_delete idx={d} failed");
        }
        let rc = (api.zip_close)(za);
        assert_eq!(rc, 0, "zip_close failed");
        t.elapsed().as_secs_f64()
    }

    // ---- Rust rewrite model ----
    fn rust_rewrite(
        files: &[ArchiveFile],
        renames: &[(u64, &str)],
        deletes: &[u64],
        opts: &CompressOptions,
    ) -> f64 {
        let delete_set: HashSet<u64> = deletes.iter().copied().collect();
        let rename_map: HashMap<u64, &str> = renames.iter().copied().collect();
        let mut modified: Vec<ArchiveFile> = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            let idx = i as u64;
            if delete_set.contains(&idx) {
                continue;
            }
            let name = rename_map
                .get(&idx)
                .map(|s| (*s).to_string())
                .unwrap_or_else(|| f.name.clone());
            modified.push(ArchiveFile::new(name, f.data.clone()));
        }
        let t = Instant::now();
        let out = write_archive(&modified, opts).expect("rust rewrite failed");
        let _keep = out.len();
        t.elapsed().as_secs_f64()
    }

    // Warmup both.
    unsafe { c_modify(&api, &arch_path, &renames, &deletes) };
    std::fs::write(&arch_path, &original_bytes).unwrap();
    let _ = rust_rewrite(&files, &renames, &deletes, &opts);

    let mut rows = String::new();
    rows.push_str(&csv_header(false));

    // C in-place.
    let mut c_secs = Vec::new();
    for run in 0..ITERS {
        std::fs::write(&arch_path, &original_bytes).unwrap(); // restore original
        let secs = unsafe { c_modify(&api, &arch_path, &renames, &deletes) };
        c_secs.push(secs);
        rows.push_str(&csv_row_wl(
            "c_libzip",
            &cver,
            "modify_inplace",
            run + 1,
            total_uncomp,
            secs,
            total_uncomp as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }
    std::fs::remove_file(&arch_path).ok();

    // Rust rewrite.
    let mut r_secs = Vec::new();
    for run in 0..ITERS {
        let secs = rust_rewrite(&files, &renames, &deletes, &opts);
        r_secs.push(secs);
        rows.push_str(&csv_row_wl(
            "rust_zip_core",
            env!("CARGO_PKG_VERSION"),
            "modify_rewrite",
            run + 1,
            total_uncomp,
            secs,
            total_uncomp as f64 / secs / (1024.0 * 1024.0),
            None,
        ));
    }

    write_csv("benchmark-modify", &rows).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    eprintln!(
        "modify: C in-place {:.4} ms vs Rust rewrite {:.4} ms ({:.1} MiB corpus; C reuses compressed data, Rust recompresses)",
        median(c_secs) * 1000.0,
        median(r_secs) * 1000.0,
        total_uncomp as f64 / (1024.0 * 1024.0),
    );
}

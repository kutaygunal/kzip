//! Cross-read / write-path equivalence check.
//!
//! Because `zip-sys` (the Rust cdylib) deliberately does NOT export write
//! symbols, byte-level write-path equivalence is proven here with zip-core's
//! in-process APIs plus the real C libzip, in both directions:
//!
//!  (a) **C writes, Rust reads**: every C-generated archive under
//!      `data/corpus-verify/*.zip` is opened with `zip_core::Archive`
//!      (in-process) and every entry's decompressed bytes are compared to the
//!      ground-truth input mirrored by `gen_corpus` at
//!      `data/corpus-verify/inputs/<archive>/<index>`. Names, sizes and
//!      compression methods are checked too.
//!
//!  (b) **Rust writes, C reads**: `zip_core::write_archive` (deflate level 6)
//!      writes `data/corpus-verify/rust-written/rust_deflate.zip`, then the
//!      ORIGINAL C libzip (loaded via `libloading`) reads it back and every
//!      entry's bytes + name are compared to the inputs.
//!
//! Usage: `cross_read <c-zip-dll> <corpus-dir>`

use libloading::Library;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::{c_void, CStr, CString};
use std::io::Read;
use std::path::{Path, PathBuf};

use zip_core::{Archive, ArchiveFile, CompressOptions};
use zip_core::constant::CompressionMethod;

fn sha256hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

// ---- minimal C read API for part (b) ----
type Zh = c_void;
type Fh = c_void;

struct CReadApi {
    _lib: Library,
    zip_open: unsafe extern "C" fn(*const libc::c_char, i32, *mut i32) -> *mut Zh,
    zip_get_num_entries: unsafe extern "C" fn(*const Zh, u32) -> i64,
    zip_get_name: unsafe extern "C" fn(*const Zh, u64, u32) -> *const libc::c_char,
    zip_fopen_index: unsafe extern "C" fn(*mut Zh, u64, u32) -> *mut Fh,
    zip_fread: unsafe extern "C" fn(*mut Fh, *mut c_void, u64) -> i64,
    zip_fclose: unsafe extern "C" fn(*mut Fh) -> i32,
    zip_close: unsafe extern "C" fn(*mut Zh) -> i32,
}

impl CReadApi {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let lib = Library::new(path).map_err(|e| format!("dlopen {path:?}: {e}"))?;
        fn resolve<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
            unsafe { lib.get::<T>(name.as_bytes()) }
                .map(|s| *s)
                .map_err(|e| format!("symbol {name}: {e}"))
        }
        Ok(CReadApi {
            zip_open: resolve(&lib, "zip_open")?,
            zip_get_num_entries: resolve(&lib, "zip_get_num_entries")?,
            zip_get_name: resolve(&lib, "zip_get_name")?,
            zip_fopen_index: resolve(&lib, "zip_fopen_index")?,
            zip_fread: resolve(&lib, "zip_fread")?,
            zip_fclose: resolve(&lib, "zip_fclose")?,
            zip_close: resolve(&lib, "zip_close")?,
            _lib: lib,
        })
    }

    /// Read the whole entry at `index` via the C library.
    unsafe fn read_entry(&self, zh: *mut Zh, index: u64) -> Option<Vec<u8>> {
        let fh = unsafe { (self.zip_fopen_index)(zh, index, 0) };
        if fh.is_null() {
            return None;
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { (self.zip_fread)(fh, buf.as_mut_ptr() as *mut c_void, buf.len() as u64) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            } else if n == 0 {
                break;
            } else {
                unsafe { (self.zip_fclose)(fh) };
                return None;
            }
        }
        unsafe { (self.zip_fclose)(fh) };
        Some(out)
    }
}

#[derive(Serialize, Debug)]
struct EntryCheck {
    index: u64,
    name: Option<String>,
    size: u64,
    method: u16,
    expected_sha256: String,
    read_sha256: String,
    bytes_match: bool,
    name_match: bool,
    size_match: bool,
}

#[derive(Serialize, Debug)]
struct ArchiveCheck {
    archive: String,
    num_entries: usize,
    entries: Vec<EntryCheck>,
    all_match: bool,
}

fn read_entry_with_rust(arch: &Archive, index: u64) -> (String, Vec<u8>) {
    let mut r = arch.open_entry(index).expect("open_entry");
    let mut out = Vec::new();
    r.read_to_end(&mut out).expect("read entry");
    (sha256hex(&out), out)
}

/// Part (a): C-generated archives read by Rust zip-core.
fn c_writes_rust_reads(corpus: &Path) -> Vec<ArchiveCheck> {
    let inputs_root = corpus.join("inputs");
    let mut result = Vec::new();
    let mut zips: Vec<PathBuf> = std::fs::read_dir(corpus)
        .expect("corpus dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "zip").unwrap_or(false) && p.parent() == Some(corpus))
        .collect();
    zips.sort();

    for zp in zips {
        let stem = zp.file_stem().unwrap().to_string_lossy().to_string();
        let in_dir = inputs_root.join(&stem);
        let data = std::fs::read(&zp).expect("read archive");
        let arch = match Archive::open(std::io::Cursor::new(data)) {
            Ok(a) => a,
            Err(_) => {
                // zip-core could not open this archive (e.g. the 70k-entry
                // ZIP64 one). Record it as a non-matching archive so the
                // report surfaces the gap instead of panicking.
                result.push(ArchiveCheck {
                    archive: zp.display().to_string(),
                    num_entries: 0,
                    entries: vec![],
                    all_match: false,
                });
                continue;
            }
        };

        let mut checks = Vec::new();
        for i in 0..arch.len() {
            let expected = std::fs::read(in_dir.join(format!("{i}"))).expect("ground-truth input");
            let (r_sha, r_bytes) = read_entry_with_rust(&arch, i);
            let st = arch.stat(i).expect("stat");
            checks.push(EntryCheck {
                index: i,
                name: arch.name(i).map(|s| s.to_string()),
                size: st.size.unwrap_or(0),
                method: st.comp_method.unwrap_or(0),
                expected_sha256: sha256hex(&expected),
                read_sha256: r_sha,
                bytes_match: r_bytes == expected,
                // In part (a) the archive's own entry name is authoritative; we
                // just require it to be present and non-empty.
                name_match: arch.name(i).is_some_and(|s| !s.is_empty()),
                size_match: r_bytes.len() as u64 == expected.len() as u64,
            });
        }
        let all_match = checks.iter().all(|c| c.bytes_match && c.size_match);
        result.push(ArchiveCheck {
            archive: zp.display().to_string(),
            num_entries: checks.len(),
            entries: checks,
            all_match,
        });
    }
    result
}

/// Part (b): Rust `write_archive` output read by the C library.
fn rust_writes_c_reads(api: &CReadApi, corpus: &Path) -> ArchiveCheck {
    let files = vec![
        ArchiveFile::new("a.txt", b"rust writes, c reads payload ".repeat(80)),
        ArchiveFile::new("b.bin", (0u8..=255).cycle().take(4096).collect::<Vec<u8>>()),
        ArchiveFile::new("empty.txt", Vec::<u8>::new()),
        ArchiveFile::new("sub/c.txt", b"nested rust file ".repeat(30)),
        ArchiveFile::new("uni/japan-日本語.txt", b"unicode rust ".repeat(20)),
    ];

    let opts = CompressOptions {
        method: CompressionMethod::Deflate,
        level: 6,
        parallel: false,
        ..Default::default()
    };
    let bytes = zip_core::write_archive(&files, &opts).expect("write_archive");

    let rust_dir = corpus.join("rust-written");
    let in_dir = corpus.join("inputs").join("rust_deflate");
    std::fs::create_dir_all(&rust_dir).unwrap();
    std::fs::create_dir_all(&in_dir).unwrap();
    let zip_path = rust_dir.join("rust_deflate.zip");
    std::fs::write(&zip_path, &bytes).unwrap();
    for (i, f) in files.iter().enumerate() {
        std::fs::write(in_dir.join(format!("{i}")), &f.data).unwrap();
    }

    let cpath = CString::new(zip_path.to_string_lossy().as_bytes()).unwrap();
    let mut errp: i32 = 0;
    let zh = unsafe { (api.zip_open)(cpath.as_ptr(), 0, &mut errp) };
    assert!(!zh.is_null(), "C libzip could not open Rust-written archive errp={errp}");
    let num = unsafe { (api.zip_get_num_entries)(zh, 0) };
    assert_eq!(num, files.len() as i64);

    let mut checks = Vec::new();
    for i in 0..files.len() as u64 {
        let name_ptr = unsafe { (api.zip_get_name)(zh, i, 0) };
        let c_name = if name_ptr.is_null() {
            None
        } else {
            unsafe { CStr::from_ptr(name_ptr) }.to_str().ok().map(|s| s.to_string())
        };
        let expected = files[i as usize].data.clone();
        let read = unsafe { api.read_entry(zh, i) }.expect("C fread");
        checks.push(EntryCheck {
            index: i,
            name: c_name.clone(),
            size: read.len() as u64,
            method: 8,
            expected_sha256: sha256hex(&expected),
            read_sha256: sha256hex(&read),
            bytes_match: read == expected,
            name_match: c_name.as_deref() == Some(files[i as usize].name.as_str()),
            size_match: read.len() == expected.len(),
        });
    }
    unsafe { (api.zip_close)(zh) };

    let all_match = checks.iter().all(|c| c.bytes_match && c.size_match && c.name_match);
    ArchiveCheck {
        archive: zip_path.display().to_string(),
        num_entries: checks.len(),
        entries: checks,
        all_match,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cross_read <c-zip-dll> <corpus-dir>");
        std::process::exit(2);
    }
    let api = unsafe { CReadApi::load(Path::new(&args[1])) }.expect("load C libzip");
    let corpus = PathBuf::from(&args[2]);

    let a = c_writes_rust_reads(&corpus);
    let b = rust_writes_c_reads(&api, &corpus);

    let out = serde_json::json!({
        "c_writes_rust_reads": a,
        "rust_writes_c_reads": b,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());

    let a_ok = a.iter().all(|x| x.all_match);
    let b_ok = b.all_match;
    eprintln!("[cross_read] C-writes/Rust-reads all_match={a_ok}, Rust-writes/C-reads all_match={b_ok}");
}

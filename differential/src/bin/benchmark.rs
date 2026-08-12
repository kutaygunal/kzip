//! Reproducible C-ABI benchmark runner for the original libzip DLL and kzip.
//!
//! This intentionally benchmarks the same exported API path in both
//! implementations. It is a throughput/latency snapshot, not a universal
//! claim about every filesystem, CPU, compiler, or workload.
//!
//! Usage:
//! `benchmark <c-dll> <rust-dll> <output-json> [--samples N] [--warmups N]`

use libloading::Library;
use serde::Serialize;
use std::ffi::{c_char, c_int, c_void, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

type Handle = c_void;
type Source = c_void;
type FileHandle = c_void;

const ZIP_CREATE: c_int = 1;
const ZIP_TRUNCATE: c_int = 8;
const ZIP_RDONLY: c_int = 16;
const ZIP_CM_STORE: c_int = 0;
const ZIP_CM_DEFLATE: c_int = 8;

#[derive(Clone, Copy)]
struct Api {
    zip_open: unsafe extern "C" fn(*const c_char, c_int, *mut c_int) -> *mut Handle,
    zip_close: unsafe extern "C" fn(*mut Handle) -> c_int,
    zip_get_num_entries: unsafe extern "C" fn(*const Handle, u32) -> i64,
    zip_source_buffer: unsafe extern "C" fn(*mut Handle, *const c_void, u64, c_int) -> *mut Source,
    zip_source_free: unsafe extern "C" fn(*mut Source),
    zip_file_add: unsafe extern "C" fn(*mut Handle, *const c_char, *mut Source, u32) -> i64,
    zip_set_file_compression: unsafe extern "C" fn(*mut Handle, u64, c_int, u32) -> c_int,
    zip_fopen_index: unsafe extern "C" fn(*mut Handle, u64, u32) -> *mut FileHandle,
    zip_fread: unsafe extern "C" fn(*mut FileHandle, *mut c_void, u64) -> i64,
    zip_fclose: unsafe extern "C" fn(*mut FileHandle) -> c_int,
    zip_libzip_version: unsafe extern "C" fn() -> *const c_char,
}

struct LoadedApi {
    _library: Library,
    api: Api,
    version: String,
}

impl LoadedApi {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let library = Library::new(path).map_err(|e| format!("load {}: {e}", path.display()))?;
        unsafe fn resolve<T: Copy>(library: &Library, name: &[u8]) -> Result<T, String> {
            library
                .get::<T>(name)
                .map(|symbol| *symbol)
                .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(name)))
        }
        let api = Api {
            zip_open: unsafe { resolve(&library, b"zip_open\0")? },
            zip_close: unsafe { resolve(&library, b"zip_close\0")? },
            zip_get_num_entries: unsafe { resolve(&library, b"zip_get_num_entries\0")? },
            zip_source_buffer: unsafe { resolve(&library, b"zip_source_buffer\0")? },
            zip_source_free: unsafe { resolve(&library, b"zip_source_free\0")? },
            zip_file_add: unsafe { resolve(&library, b"zip_file_add\0")? },
            zip_set_file_compression: unsafe { resolve(&library, b"zip_set_file_compression\0")? },
            zip_fopen_index: unsafe { resolve(&library, b"zip_fopen_index\0")? },
            zip_fread: unsafe { resolve(&library, b"zip_fread\0")? },
            zip_fclose: unsafe { resolve(&library, b"zip_fclose\0")? },
            zip_libzip_version: unsafe { resolve(&library, b"zip_libzip_version\0")? },
        };
        let version_ptr = unsafe { (api.zip_libzip_version)() };
        let version = if version_ptr.is_null() {
            "unknown".to_string()
        } else {
            unsafe { std::ffi::CStr::from_ptr(version_ptr) }
                .to_string_lossy()
                .into_owned()
        };
        Ok(LoadedApi {
            _library: library,
            api,
            version,
        })
    }
}

#[derive(Clone)]
struct Workload {
    name: &'static str,
    files: Vec<Vec<u8>>,
    total_bytes: u64,
}

#[derive(Serialize)]
struct BenchmarkFile {
    generated_at: String,
    host: HostInfo,
    c_library: String,
    rust_library: String,
    samples: usize,
    warmups: usize,
    workloads: Vec<WorkloadInfo>,
    results: Vec<Measurement>,
}

#[derive(Serialize)]
struct HostInfo {
    os: &'static str,
    arch: &'static str,
    rustc: String,
}

#[derive(Serialize)]
struct WorkloadInfo {
    name: String,
    files: usize,
    bytes: u64,
}

#[derive(Serialize, Clone)]
struct Measurement {
    engine: String,
    operation: String,
    workload: String,
    method: String,
    files: usize,
    bytes: u64,
    samples_ns: Vec<u128>,
    median_ns: u128,
    p95_ns: u128,
    throughput_mib_s: f64,
    checksum: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("benchmark failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return Err(
            "usage: benchmark <c-dll> <rust-dll> <output-json> [--samples N] [--warmups N]"
                .to_string(),
        );
    }
    let c_path = PathBuf::from(&args[1]);
    let rust_path = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    let samples = option_value(&args, "--samples", 5).max(1);
    let warmups = option_value(&args, "--warmups", 1);

    let c = unsafe { LoadedApi::load(&c_path) }?;
    let rust = unsafe { LoadedApi::load(&rust_path) }?;
    let workloads = workloads();
    let root = std::env::temp_dir().join(format!("kzip-benchmark-{}", std::process::id()));
    fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;

    let mut results = Vec::new();
    for workload in &workloads {
        for method in [(ZIP_CM_STORE, "store"), (ZIP_CM_DEFLATE, "deflate")] {
            let canonical = root.join(format!("canonical-{}-{}.zip", workload.name, method.1));
            write_archive(&c.api, workload, method.0, &canonical, false)?;
            let (expected, canonical_checksum) =
                read_archive_checked(&c.api, &canonical, workload)?;
            if expected != workload.total_bytes {
                return Err(format!(
                    "canonical archive {} read {} bytes, expected {}",
                    canonical.display(),
                    expected,
                    workload.total_bytes
                ));
            }
            let expected_checksum = workload_checksum(workload);
            if canonical_checksum != expected_checksum {
                return Err(format!(
                    "canonical archive {} checksum {canonical_checksum}, expected {expected_checksum}",
                    canonical.display()
                ));
            }

            for (engine, loaded) in [("libzip-c", &c), ("kzip-rust", &rust)] {
                let write_path =
                    root.join(format!("write-{engine}-{}-{}.zip", workload.name, method.1));
                let write = measure(samples, warmups, || {
                    write_archive(
                        &loaded.api,
                        workload,
                        method.0,
                        &write_path,
                        engine == "kzip-rust",
                    )
                })?;
                let write_checksum =
                    verify_archive(&loaded.api, &write_path, workload, expected_checksum)?;
                results.push(measurement(
                    engine,
                    "write",
                    workload,
                    method.1,
                    write,
                    write_checksum,
                ));

                let read = measure(samples, warmups, || {
                    let (bytes, checksum) =
                        read_archive_checked(&loaded.api, &canonical, workload)?;
                    if bytes != workload.total_bytes {
                        return Err(format!("read byte count mismatch: {bytes}"));
                    }
                    if checksum != expected_checksum {
                        return Err(format!(
                            "read checksum {checksum}, expected {expected_checksum}"
                        ));
                    }
                    Ok(checksum)
                })?;
                results.push(measurement(
                    engine,
                    "read",
                    workload,
                    method.1,
                    read,
                    canonical_checksum,
                ));
            }
        }
    }

    let generated_at = chrono_like_timestamp();
    let document = BenchmarkFile {
        generated_at,
        host: HostInfo {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            rustc: rustc_version(),
        },
        c_library: c.version,
        rust_library: rust.version,
        samples,
        warmups,
        workloads: workloads
            .iter()
            .map(|w| WorkloadInfo {
                name: w.name.to_string(),
                files: w.files.len(),
                bytes: w.total_bytes,
            })
            .collect(),
        results,
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(&document).map_err(|e| format!("serialize JSON: {e}"))?;
    fs::write(&output, json).map_err(|e| format!("write {}: {e}", output.display()))?;
    println!("wrote {}", output.display());
    let _ = fs::remove_dir_all(root);
    Ok(())
}

fn option_value(args: &[String], option: &str, default: usize) -> usize {
    args.windows(2)
        .find(|pair| pair[0] == option)
        .and_then(|pair| pair[1].parse().ok())
        .unwrap_or(default)
}

fn measure<F>(samples: usize, warmups: usize, mut operation: F) -> Result<Vec<u128>, String>
where
    F: FnMut() -> Result<u64, String>,
{
    for _ in 0..warmups {
        operation()?;
    }
    let mut durations = Vec::with_capacity(samples);
    let mut checksums = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let checksum = operation()?;
        durations.push(start.elapsed().as_nanos());
        checksums.push(checksum);
    }
    if checksums.windows(2).any(|pair| pair[0] != pair[1]) {
        return Err("checksum changed between benchmark samples".to_string());
    }
    Ok(durations)
}

fn measurement(
    engine: &str,
    operation: &str,
    workload: &Workload,
    method: &str,
    samples: Vec<u128>,
    checksum: u64,
) -> Measurement {
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let median_ns = sorted[sorted.len() / 2];
    let p95_index = ((sorted.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    let p95_ns = sorted[p95_index];
    let throughput_mib_s = workload.total_bytes as f64 / (median_ns as f64 / 1e9) / 1_048_576.0;
    Measurement {
        engine: engine.to_string(),
        operation: operation.to_string(),
        workload: workload.name.to_string(),
        method: method.to_string(),
        files: workload.files.len(),
        bytes: workload.total_bytes,
        samples_ns: samples,
        median_ns,
        p95_ns,
        throughput_mib_s,
        checksum,
    }
}

fn write_archive(
    api: &Api,
    workload: &Workload,
    method: c_int,
    path: &Path,
    free_source_after_add: bool,
) -> Result<u64, String> {
    let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let handle = unsafe {
        (api.zip_open)(
            path.as_ptr(),
            ZIP_CREATE | ZIP_TRUNCATE,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() {
        return Err("zip_open failed for write".to_string());
    }
    for (index, data) in workload.files.iter().enumerate() {
        let name = CString::new(format!("f-{index:05}.bin")).map_err(|e| e.to_string())?;
        let source =
            unsafe { (api.zip_source_buffer)(handle, data.as_ptr().cast(), data.len() as u64, 0) };
        if source.is_null() {
            unsafe { (api.zip_close)(handle) };
            return Err(format!("zip_source_buffer failed for entry {index}"));
        }
        let entry = unsafe { (api.zip_file_add)(handle, name.as_ptr(), source, 0) };
        if entry < 0 {
            unsafe { (api.zip_source_free)(source) };
            unsafe { (api.zip_close)(handle) };
            return Err(format!("zip_file_add failed for entry {index}"));
        }
        if free_source_after_add {
            unsafe { (api.zip_source_free)(source) };
        }
        if unsafe { (api.zip_set_file_compression)(handle, entry as u64, method, 6) } != 0 {
            unsafe { (api.zip_close)(handle) };
            return Err(format!("zip_set_file_compression failed for entry {index}"));
        }
        // libzip takes ownership of a successful source. kzip snapshots the
        // source immediately; retaining the pointer here keeps both ABI paths
        // on the same ownership contract during the timed close.
    }
    if unsafe { (api.zip_close)(handle) } != 0 {
        return Err("zip_close failed for write".to_string());
    }
    Ok(workload.total_bytes)
}

fn verify_archive(
    api: &Api,
    path: &Path,
    workload: &Workload,
    expected_checksum: u64,
) -> Result<u64, String> {
    let (bytes, checksum) = read_archive_checked(api, path, workload)?;
    if bytes != workload.total_bytes {
        return Err(format!(
            "written archive contains {bytes} bytes, expected {}",
            workload.total_bytes
        ));
    }
    if checksum != expected_checksum {
        return Err(format!(
            "written archive checksum {checksum}, expected {expected_checksum}"
        ));
    }
    Ok(checksum)
}

fn read_archive_checked(api: &Api, path: &Path, workload: &Workload) -> Result<(u64, u64), String> {
    let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let handle = unsafe { (api.zip_open)(path.as_ptr(), ZIP_RDONLY, std::ptr::null_mut()) };
    if handle.is_null() {
        return Err(format!(
            "zip_open failed for read: {}",
            path.to_string_lossy()
        ));
    }
    let count = unsafe { (api.zip_get_num_entries)(handle, 0) };
    if !workload.files.is_empty() && count != workload.files.len() as i64 {
        unsafe { (api.zip_close)(handle) };
        return Err(format!(
            "entry count {count}, expected {}",
            workload.files.len()
        ));
    }
    let mut bytes = 0u64;
    let mut checksum = 14_695_981_039_346_656_037u64;
    let mut buffer = vec![0u8; 64 * 1024];
    for index in 0..count.max(0) as u64 {
        let file = unsafe { (api.zip_fopen_index)(handle, index, 0) };
        if file.is_null() {
            unsafe { (api.zip_close)(handle) };
            return Err(format!("zip_fopen_index failed for entry {index}"));
        }
        loop {
            let read =
                unsafe { (api.zip_fread)(file, buffer.as_mut_ptr().cast(), buffer.len() as u64) };
            if read < 0 {
                unsafe { (api.zip_fclose)(file) };
                unsafe { (api.zip_close)(handle) };
                return Err(format!("zip_fread failed for entry {index}"));
            }
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read as usize];
            bytes += read as u64;
            for byte in chunk {
                checksum ^= *byte as u64;
                checksum = checksum.wrapping_mul(1_099_511_628_211);
            }
        }
        if unsafe { (api.zip_fclose)(file) } != 0 {
            unsafe { (api.zip_close)(handle) };
            return Err(format!("zip_fclose failed for entry {index}"));
        }
    }
    if unsafe { (api.zip_close)(handle) } != 0 {
        return Err("zip_close failed for read".to_string());
    }
    Ok((bytes, checksum))
}

fn workloads() -> Vec<Workload> {
    vec![
        workload("tiny-mixed", &[4 * 1024; 64], 0x1234_5678, true),
        workload("many-small", &[4 * 1024; 1024], 0x2345_6789, true),
        workload("text-8m", &[1024 * 1024; 8], 0x3456_789A, true),
        workload("mixed-8m", &[1024 * 1024; 8], 0x4567_89AB, false),
        workload("single-16m", &[16 * 1024 * 1024], 0x5678_9ABC, true),
    ]
}

fn workload(name: &'static str, sizes: &[usize], seed: u64, compressible: bool) -> Workload {
    let files = sizes
        .iter()
        .enumerate()
        .map(|(index, &size)| {
            if compressible || index % 2 == 0 {
                pattern_bytes(size, seed.wrapping_add(index as u64))
            } else {
                random_bytes(size, seed.wrapping_add(index as u64))
            }
        })
        .collect::<Vec<_>>();
    let total_bytes = files.iter().map(|file| file.len() as u64).sum();
    Workload {
        name,
        files,
        total_bytes,
    }
}

fn pattern_bytes(size: usize, seed: u64) -> Vec<u8> {
    let phrase = format!("kzip benchmark pattern {seed:016x} ");
    phrase
        .as_bytes()
        .iter()
        .copied()
        .cycle()
        .take(size)
        .collect()
}

fn random_bytes(size: usize, mut state: u64) -> Vec<u8> {
    let mut output = Vec::with_capacity(size);
    while output.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        output.extend_from_slice(&state.to_le_bytes());
    }
    output.truncate(size);
    output
}

fn workload_checksum(workload: &Workload) -> u64 {
    let mut checksum = 14_695_981_039_346_656_037u64;
    for file in &workload.files {
        for byte in file {
            checksum ^= *byte as u64;
            checksum = checksum.wrapping_mul(1_099_511_628_211);
        }
    }
    checksum
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| version.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn chrono_like_timestamp() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

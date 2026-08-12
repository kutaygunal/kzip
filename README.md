# kzip

`kzip` is an independent Rust implementation of the core ideas and public ZIP
workflow of [libzip](https://libzip.org/). It can read, create, inspect, and
modify ZIP archives, and it provides both an idiomatic Rust API and a
libzip-shaped C ABI layer.

This is a compatibility-oriented project, not a claim that the Rust library is
the original libzip or a complete replacement for every libzip release.

## What is in the repository

| Crate | Role |
| --- | --- |
| `zip-core` | Safe Rust engine for ZIP parsing, reading, writing, editing, codecs, and encryption. |
| `zip-async` | Tokio adapter for asynchronous archive entry reads. |
| `zip-sys` | `cdylib` exposing the implemented `zip_*` C ABI surface and `zip.h`. |
| `ziptools` | Rust archive comparison tool, including the `zipcmp`-style CLI. |
| `differential` | Verification harness that compares the Rust ABI with a C libzip build. |
| `fuzz` | Standalone cargo-fuzz targets for malformed archive and codec input. |

## Features

- Read and write standard ZIP archives, including stored and DEFLATE entries.
- Bzip2 decoding through the Rust codec layer.
- ZIP64 reading, central-directory validation, CRC/size checks, and typed errors.
- ZipCrypto and WinZip AES-128, AES-192, and AES-256 read/write support.
- Archive editing: add, replace, delete, rename, comments, extra fields, and
  metadata through the supported APIs.
- Optional deterministic parallel compression of independent files.
- Zero-copy-friendly sources, buffer reuse, memory mapping, and a Tokio read
  adapter.
- A C ABI layer for applications that use the supported subset of libzip's
  `zip_*` API.

The core crate is written with `#![deny(unsafe_code)]`. The FFI crate necessarily
contains the unsafe boundary required to expose C pointers and handles.

## Rust example

```rust
use std::io::{Cursor, Read};
use zip_core::{write_archive, Archive, ArchiveFile, CompressOptions, Result};

fn main() -> Result<()> {
    let files = vec![
        ArchiveFile::new("hello.txt", b"Hello from kzip".to_vec()),
        ArchiveFile::new("data.bin", vec![0; 4096]),
    ];

    let bytes = write_archive(&files, &CompressOptions::default())?;
    let archive = Archive::open(Cursor::new(bytes))?;

    let mut reader = archive.open_entry(0)?;
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    assert_eq!(output, b"Hello from kzip");
    Ok(())
}
```

Add the core crate to an application with:

```sh
cargo add zip-core
```

For file-level changes, `modify_archive` returns modified bytes and
`modify_archive_file` updates an archive on disk while reusing existing member
data where possible. The migration guide maps common libzip calls to their Rust
counterparts: [`docs/migration.md`](docs/migration.md).

## kzip compared with libzip

The local reference used by this repository is C libzip **1.11.4**. The
comparison below describes capabilities, not runtime speed:

| Area | Original libzip | kzip |
| --- | --- | --- |
| Implementation | Portable C library with its complete public C API. | Rust workspace with `zip-core`, an async adapter, tools, and an FFI layer. |
| Rust API | Not applicable. | Ownership-based `Result` API over `Read`/`Seek` sources. |
| C API | Full libzip API for the release. | Implemented compatibility subset in `zip-sys`; see [`docs/ABI.md`](docs/ABI.md). |
| Archive operations | Read, create, modify, revert, and close/write. | Read, create, modify, comments, metadata, and supported revert/edit operations. |
| ZIP formats | ZIP and ZIP64 read/write. | ZIP and ZIP64 reading; ZIP64 writing remains an open compatibility gap. |
| Codecs | Store, DEFLATE, Bzip2, LZMA, and Zstandard when configured. | Store, DEFLATE, and Bzip2; LZMA and Zstandard are not currently implemented. |
| Encryption | Traditional PKWARE encryption and WinZip AES. | Traditional PKWARE encryption and WinZip AES-128/192/256. |
| Sources | Broad `zip_source_*` family, including file, callback, layered, and window sources. | Rust `Source` abstractions and the implemented `zip-sys` source functions; not every libzip source symbol is exported. |
| Parallelism | Library API is primarily synchronous; applications choose their own concurrency. | Optional Rayon-based parallel compression with deterministic archive ordering. |
| Safety model | C callers own memory and handle lifetime. | Safe core APIs; C callers still need normal FFI ownership and lifetime discipline. |

Known ABI differences are intentionally documented rather than hidden. In
addition to ZIP64 writing, the current compatibility layer still has a small
number of missing or legacy-only entry points, such as `zip_file_rename`,
`zip_source_buffer_create`, `zip_error_to_data`, and deprecated add/replace
aliases. Treat `zip-sys` as a supported subset until the exact header and
symbol status says otherwise.

## Comparing behavior with the original C library

The repository includes a differential harness for behavioral verification. It
compares read/stat/error results and performs cross-reading of archives written
by each implementation. Timing is not treated as evidence of correctness.

The verification workflow expects a C libzip 1.11.4 DLL at
`libs/c/zip.dll` and Bash on the path:

```sh
cargo test --workspace
bash run-verify.sh
```

The normal Rust checks are:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --no-deps --workspace
```

The detailed capability mapping is in [`docs/support-matrix.md`](docs/support-matrix.md),
the C ABI declaration is [`crates/zip-sys/include/zip.h`](crates/zip-sys/include/zip.h),
and the differential-test notes are in [`docs/regress.md`](docs/regress.md).

## Project status

The project is under active development. The most important compatibility work
is tracked in source tests and the differential harness. APIs may change before
the crates are considered stable; `zip-core` is currently the only crate
intended for publication.

## License

BSD-3-Clause. See [`LICENSE`](LICENSE). kzip is an independent implementation
that acknowledges the libzip project and its authors, Dieter Baron and Thomas
Klausner.

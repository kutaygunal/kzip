# kzip

[![CI](https://github.com/kutaygunal/kzip/actions/workflows/ci.yml/badge.svg)](https://github.com/kutaygunal/kzip/actions/workflows/ci.yml)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)
[![MSRV: 1.75](https://img.shields.io/badge/MSRV-1.75-orange.svg)](docs/msrv.md)

**Fast, memory-safe ZIP infrastructure for Rust and C.**

`kzip` gives applications a practical way to read, create, inspect, modify,
and encrypt ZIP archives without forcing every integration to choose between
Rust ergonomics and a familiar libzip-shaped C interface. The project combines
a safe Rust core, an async Tokio adapter, and a documented C ABI subset.

kzip is an independent implementation inspired by [libzip](https://libzip.org/).
It is compatibility-oriented, not the original libzip and not yet a complete
drop-in replacement for every libzip release.

> **Project status:** active development. The APIs are useful today, but may
> change before the crates are considered stable. Compatibility claims are
> backed by tests and differential verification rather than by API shape alone.

## Why kzip?

ZIP handling sits on an application’s trust boundary: archives can be large,
malformed, encrypted, or produced by another language. kzip is designed to
make that boundary easier to own:

- **Safe Rust by default.** `zip-core` denies `unsafe_code`, returns typed
  errors, and validates archive structure, sizes, and checksums.
- **A bridge for existing systems.** `zip-sys` exposes the supported
  libzip-shaped `zip_*` surface as a C-compatible dynamic library.
- **Performance you can inspect.** Read-only file opens can use memory mapping,
  Store entries can follow a zero-copy-friendly path, and independent files can
  be compressed in parallel with deterministic output.
- **A complete workflow, not just decompression.** Create archives, edit
  members, preserve metadata, stream entries, and use ZipCrypto or WinZip AES
  when encrypted archives are part of the product.
- **Evidence close to the implementation.** Rust tests, robustness tests,
  fuzz targets, and a C-vs-Rust differential harness make behavior easier to
  review and extend.

## What is in the repository

| Crate | Role |
| --- | --- |
| `zip-core` | Safe Rust engine for ZIP parsing, reading, writing, editing, codecs, and encryption. |
| `zip-async` | Tokio adapter for asynchronous archive entry reads. |
| `zip-sys` | `cdylib` exposing the implemented `zip_*` C ABI surface and `zip.h`. |
| `ziptools` | Rust archive comparison tool, including the `zipcmp`-style CLI. |
| `differential` | Verification harness that compares the Rust ABI with a C libzip build. |
| `fuzz` | Standalone cargo-fuzz targets for malformed archive and codec input. |

## Product capabilities

- Read and write standard ZIP archives, including Store and DEFLATE entries.
- Decode Bzip2 through the Rust codec layer.
- Read ZIP64 archives with central-directory validation, CRC/size checks, and
  typed errors.
- Read and write ZipCrypto and WinZip AES-128, AES-192, and AES-256 archives.
- Edit archives by adding, replacing, deleting, and renaming entries, while
  working with comments, extra fields, and metadata through the supported APIs.
- Compress independent files in parallel with deterministic archive ordering.
- Use zero-copy-friendly sources, buffer reuse, memory mapping, and a Tokio
  adapter for asynchronous entry reads.
- Integrate from C through the supported subset of libzip's `zip_*` API.

## Engineering philosophy

This project is heavily inspired by the recent [Bun Zig-to-Rust rewrite](https://github.com/oven-sh/bun/pull/30412): preserve the behavior and architecture of a systems project while moving the implementation toward Rust's ownership model and compiler-assisted safety. The porting mindset here is similar—map the original API, keep compatibility evidence close to the code, and use the original implementation as the behavioral oracle.

This repository is openly **vibe-coded**. AI-assisted generation was part of
the implementation process. The practical contract is therefore the one
enforced by compilation, tests, fuzz/robustness checks, and C-vs-Rust
differential verification—not an assumption that generated code is correct by
itself.

The core crate is written with `#![deny(unsafe_code)]`. The FFI crate necessarily
contains the unsafe boundary required to expose C pointers and handles; that
boundary is kept separate from the safe archive engine.

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

While the first crates.io release is being prepared, add the core crate directly
from GitHub:

```sh
cargo add zip-core --git https://github.com/kutaygunal/kzip
```

Once `zip-core` is published, the dependency can be shortened to
`cargo add zip-core`.

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
| C API | Full libzip API for the release. | The DLL exports the 128 enumerated C functions; the shipped header is still being expanded to declare the complete surface. |
| Archive operations | Read, create, modify, revert, and close/write. | Read, create, modify, comments, metadata, and supported revert/edit operations. |
| ZIP formats | ZIP and ZIP64 read/write. | ZIP and ZIP64 read/write, including ZIP64 entry counts, overflowing sizes, offsets, and AES data descriptors. |
| Codecs | Store, DEFLATE, Bzip2, LZMA, and Zstandard when configured. | Store, DEFLATE, and Bzip2; LZMA and Zstandard are not currently implemented. |
| Encryption | Traditional PKWARE encryption and WinZip AES. | Traditional PKWARE encryption and WinZip AES-128/192/256. |
| Sources | Broad `zip_source_*` family, including file, callback, layered, and window sources. | Rust `Source` abstractions and the implemented `zip-sys` source functions; not every libzip source symbol is exported. |
| Parallelism | Library API is primarily synchronous; applications choose their own concurrency. | Optional Rayon-based parallel compression with deterministic archive ordering. |
| Safety model | C callers own memory and handle lifetime. | Safe core APIs; C callers still need normal FFI ownership and lifetime discipline. |

Known ABI differences are intentionally documented rather than hidden. The
compatibility layer still differs from a complete libzip build in optional
codec availability, some backend-specific flags, and the breadth of source
constructors. Treat `zip-sys` as a supported subset and use the shipped header
as the exact symbol contract.

## Performance snapshot

The benchmark runner compares the original C libzip DLL and the Rust DLL
through the same C ABI. It uses deterministic workloads, two warmup rounds,
seven measured samples, median/p95 latency, throughput, and checksum
validation for every archive read. Higher bars mean more MiB/s.

![Static kzip versus libzip benchmark](docs/benchmarks/benchmark.svg)

The chart is intentionally static so it renders consistently in GitHub,
package documentation, terminals, and other constrained Markdown viewers. This
snapshot was captured on Windows x86_64 with Rust 1.97.1 and libzip 1.11.4. It
is a machine-specific snapshot, not a universal performance claim.

### What the snapshot says

- Rust won 9 of 10 write rows in this run; mixed DEFLATE was the exception.
- Rust also won all 10 read rows after the read-path optimization. The
  many-small Store case improved from roughly 7 MiB/s to 805 MiB/s, versus
  385 MiB/s for C.
- The benchmark validates content checksums while timing, so a fast incorrect
  archive cannot score as a win.

### Bottleneck analysis and fixes

The original Rust read path eagerly decoded each entry into a new `Vec`, then
copied it again through `zip_fread`. That was especially expensive for 1,024
small Store entries. Read-only opens now use mmap, Store readers reference
shared immutable archive bytes directly, and DEFLATE entries stream through the
decoder (using the pooled fast path or streaming fallback) instead of calling
`read_entry` and then copying through the FFI handle. The write path also avoids
a second copy when `zip_file_add` consumes a plain buffer source and
preallocates bounded DEFLATE output capacity. The optimized snapshot beats C in 19 of 20
rows; the only remaining miss is mixed DEFLATE write, where this run measured
85.2 MiB/s Rust versus 86.8 MiB/s C.

<details>
<summary>Exact median throughput from the captured run</summary>

| Operation | Method | Workload | libzip C | kzip Rust | Faster |
| --- | --- | --- | ---: | ---: | --- |
| write | Store | tiny-mixed | 16.7 | 19.9 | Rust |
| read | Store | tiny-mixed | 386.5 | 606.4 | Rust |
| write | DEFLATE | tiny-mixed | 27.2 | 42.3 | Rust |
| read | DEFLATE | tiny-mixed | 312.5 | 427.5 | Rust |
| write | Store | many-small | 106.4 | 255.9 | Rust |
| read | Store | many-small | 385.4 | 804.9 | Rust |
| write | DEFLATE | many-small | 101.5 | 176.9 | Rust |
| read | DEFLATE | many-small | 333.4 | 509.6 | Rust |
| write | Store | text-8m | 410.2 | 713.3 | Rust |
| read | Store | text-8m | 681.4 | 879.3 | Rust |
| write | DEFLATE | text-8m | 214.9 | 546.3 | Rust |
| read | DEFLATE | text-8m | 641.1 | 974.1 | Rust |
| write | Store | mixed-8m | 382.2 | 691.1 | Rust |
| read | Store | mixed-8m | 624.0 | 831.1 | Rust |
| write | DEFLATE | mixed-8m | 86.8 | 85.2 | C |
| read | DEFLATE | mixed-8m | 535.4 | 894.9 | Rust |
| write | Store | single-16m | 507.9 | 826.3 | Rust |
| read | Store | single-16m | 628.6 | 756.5 | Rust |
| write | DEFLATE | single-16m | 195.6 | 502.0 | Rust |
| read | DEFLATE | single-16m | 636.0 | 910.6 | Rust |

Throughput is MiB/s; the underlying JSON also preserves p95 latency and all
individual samples.

</details>

### Reproduce it

```powershell
cargo build --release -p differential -p zip-sys
cargo run --release -p differential --bin benchmark -- `
  libs/c/zip.dll target/release/zip.dll `
  results/benchmark-$(Get-Date -Format yyyy-MM-dd).json `
  --samples 7 --warmups 2
python benchmarks/render.py `
  results/benchmark-$(Get-Date -Format yyyy-MM-dd).json `
  docs/benchmarks/benchmark.svg
```

The runner covers tiny mixed files, many small files, compressible text,
mixed compressibility, and a single large file across Store and DEFLATE read
and write paths. The raw run is preserved in
[`results/benchmark-2026-08-12.json`](results/benchmark-2026-08-12.json), and
the renderer is [`benchmarks/render.py`](benchmarks/render.py).

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

## Contributing

kzip is being built in the open, and contributions are welcome. The most useful
contributions are focused, test-backed improvements that make ZIP behavior
more compatible, more observable, or easier to use.

Good places to start:

- Add a regression test for a ZIP feature, malformed input, or a behavior that
  differs from libzip.
- Improve the Rust API, Tokio adapter, documentation, examples, or error
  messages.
- Extend the supported C ABI surface and update the matching header,
  differential coverage, and support documentation together.
- Improve codecs, archive editing, performance, fuzz coverage, or platform
  support with a reproducible benchmark or test case.

Before opening a pull request, run the same core checks used by CI:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
```

For behavior changes, include the smallest test that explains the intended
contract. For compatibility work, also update the relevant entry in the
[support matrix](docs/support-matrix.md) or [regression notes](docs/regress.md).
If you are unsure where a change belongs, open an [issue](https://github.com/kutaygunal/kzip/issues)
with the use case, expected behavior, and a minimal archive or reproduction.

## Project status

The project is under active development. `zip-core` is currently the only crate
intended for publication. The C ABI remains a documented supported subset, and
the following limitations are important when evaluating kzip:

- LZMA and Zstandard codecs are not currently implemented.
- `zip-async` currently bridges the synchronous engine through Tokio's blocking
  pool; a fully native async codec path is future work.
- `zip-sys` is not a complete replacement for every libzip symbol or source
  constructor. Use the shipped header and [ABI status](docs/ABI.md) as the
  exact compatibility contract.

The most important compatibility work is tracked in source tests and the
differential harness. If kzip is a fit for your application, feedback from real
archives and production-shaped workloads will help prioritize the roadmap.

## License

BSD-3-Clause. See [`LICENSE`](LICENSE). kzip is an independent implementation
that acknowledges the libzip project and its authors, Dieter Baron and Thomas
Klausner.

# kzip

A **from-scratch, memory-safe Rust reimplementation of [libzip](https://libzip.org/)**,
drop-in ABI-compatible at the `zip_*` boundary. kzip reads, creates, and modifies ZIP
archives in pure safe Rust, and ships a `zip-sys` cdylib so existing C consumers link
unchanged.

## What kzip is

- **Memory safety by construction.** The engine in `zip-core` is `#![deny(unsafe_code)]` —
  no `unsafe` in the core. Malformed input is rejected with typed `ZipError`s instead of
  crashing, and the corpus is fuzzed for the no-panic invariant (see
  [Fuzzing & hardening](fuzzing.md)).
- **Drop-in C ABI.** `zip-sys` exports libzip's `zip_*` symbols (`zip_open`, `zip_fopen`,
  `zip_stat`, …) from a `cdylib` and ships a matching `zip.h`. See [C ABI / FFI status](ABI.md).
- **Byte-for-byte compatibility.** Deterministic output that matches C libzip **1.11.4** on
  the equivalence corpus. See [libzip regress mapping](regress.md).
- **Speed.** Deterministic parallel compression over independent files plus a zero-copy
  decode path — on the benchmark machine ~16× faster than C on multi-file compression,
  ~2× on full-archive reads, ~8× on true in-place modify. See [Benchmarks](benchmarks.md).

The workspace (see the [Support matrix](support-matrix.md)):

| Crate | Purpose |
|-------|---------|
| `zip-core` | Safe engine: read / write / modify, compress, encrypt, stream sources |
| `zip-async` | Tokio adapter exposing the engine behind `AsyncRead` |
| `zip-sys` | C-ABI `cdylib` + generated `zip.h`, drop-in with libzip |
| `ziptools` | `zipcmp`-style CLI tools (ships as `kzipcmp.exe` on Windows) |
| `differential` | C-vs-Rust equivalence harness (see [regress](regress.md)) |

## Quickstart — Rust

Add `zip-core`:

```sh
cargo add zip-core
```

Build a couple of entries, write a ZIP, then open it back and read an entry:

```rust
use std::io::{Cursor, Read};
use zip_core::{write_archive, Archive, ArchiveFile, CompressOptions, Result};

fn main() -> Result<()> {
    // Two entries, written as a DEFLATE-6, parallel archive in memory.
    let files = vec![
        ArchiveFile::new("hello.txt", b"Hello from kzip!".to_vec()),
        ArchiveFile::new("data.bin", vec![0u8; 4096]),
    ];
    let zip_bytes = write_archive(&files, &CompressOptions::default())?;

    // Open the bytes back as an archive and read the first entry.
    let archive = Archive::open(Cursor::new(zip_bytes))?;
    assert_eq!(archive.len(), 2);

    let mut reader = archive.open_entry(0)?; // index 0 == "hello.txt"
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    assert_eq!(out, b"Hello from kzip!");
    Ok(())
}
```

In-place modification reuses compressed member bytes and only rewrites the central
directory + end-of-central-directory tail:

```rust
use std::path::Path;
use zip_core::{modify_archive, modify_archive_file};

// Rename entry 0 and delete entry 1, producing new bytes in memory.
let patched = modify_archive(&zip_bytes, &[(0, "renamed.txt".to_string())], &[1])?;

// Same operation, truly in place on a file on disk.
modify_archive_file(Path::new("out.zip"), &[(0, "renamed.txt".to_string())], &[1])?;
```

## Quickstart — C ABI

Link against the `zip-sys` cdylib (`kzip.dll` on Windows, `zip.dll` from `cargo build`) and
include the generated header. See [C ABI / FFI status](ABI.md) for a full example and the
exact exported symbol set.

## Chapters

- **[Migration guide](migration.md)** — moving from C libzip to kzip, path by path.
- **[C ABI / FFI status](ABI.md)** — exported `zip_*` symbols, building the cdylib.
- **[Benchmarks](benchmarks.md)** — C-vs-Rust numbers and charts.
- **[Fuzzing & hardening](fuzzing.md)** — the no-panic-on-malformed-input posture.
- **[libzip regress mapping](regress.md)** — equivalence vs libzip 1.11.4.
- **[Support matrix](support-matrix.md)** — platforms, codecs, feature flags.
- **[MSRV & Release Policy](msrv.md)** — Rust 1.75 floor and release flow.

## Build & test

```sh
cargo build --workspace
cargo test --workspace
```

The `cargo test --workspace` suite includes unit, integration, FFI, and robustness
(no-panic-on-malformed-input) tests. The differential/equivalence check is
`bash run-verify.sh` (see [regress](regress.md)).

## License

BSD-3-Clause. See `PLAN.md` for the original libzip attribution and the independent
implementation note.

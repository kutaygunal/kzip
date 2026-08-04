# Migration Guide: libzip (C) → LibzipInRust (Rust)

This guide helps existing libzip C consumers migrate to the Rust port. There
are two migration paths, depending on whether you can recompile:

- **Drop-in (no source change):** link the `zip-sys` cdylib, which exports the
  same `zip_*` C ABI as libzip. See [C ABI / FFI status](ABI.md).
- **Idiomatic Rust:** use `zip-core` (and `zip-async` for streaming) directly.
  This is the recommended path for new code.

The idiomatic API maps libzip's model onto Rust's ownership and `io` traits:

## Archive lifecycle

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_open(path, flags, &err)` | `Archive::open(path)?` |
| `zip_fdopen` | `Archive::open_from(reader, Options::default())?` |
| `zip_close(z)` | drop the `Archive` (or call the writer's `finish()`) |
| `zip_discard(z)` | drop without committing |

```rust
use zip_core::Archive;

let ar = Archive::open(Path::new("a.zip"))?; // or Archive::open_from(reader, ..)
```

## Reading entries

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_get_num_entries` | `ar.entries()` |
| `zip_get_name` | `entry.name()?` |
| `zip_fopen` / `zip_fopen_index` | `entry.reader()?` |
| `zip_fread` | `io::Read` on the reader |
| `zip_stat` | `entry.stat()` / `Source::stat()` |

```rust
use std::io::{self, Read, Seek};

for entry in ar.entries() {
    let name = entry.name()?;
    let mut r: impl Read + Seek = entry.reader()?; // zero-copy capable
    io::copy(&mut r, &mut io::stdout())?;
}
```

## Writing (parallel)

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_source_buffer_create` + `zip_file_add` | `ArchiveFile::new(name, bytes)` |
| `zip_set_file_compression` | `CompressOptions { method, level, .. }` |
| `zip_close` | `compress_files` / `write_archive` |
| manual thread pool | `CompressOptions { parallel: true, workers }` |

```rust
use zip_core::{compress_files, ArchiveFile, CompressOptions};

let mut w = /* see ArchiveWriter for streaming */;
w.add_file("photos/a.jpg", Path::new("a.jpg"))?;
w.add_bytes("notes.txt", bytes)?;
w.finish()?;
```

## Error handling

libzip's two-axis error space (`zip_error_t` with a `zip_error_code` and a
system `errno`) maps to `zip_core::ZipError`, which carries `ZipErrorCode` and a
system error. Prefer `Result`/`?` over checking error codes manually; the
library is panic-free on malformed input.

## What is *not* yet migrated

- **Async streaming:** use `zip-async` (bridge mode) for non-blocking I/O.
- **Full C ABI parity:** the `zip-sys` FFI layer covers the read/write surface;
  check [ABI.md](ABI.md) and the generated `zip.h` for the exact symbol set.
- **Codecs** Zstd/LZMA are not yet enabled in the C baseline (see the codec
  matrix in `results/C-BASELINE.md`); DEFLATE and Bzip2 are available in both.

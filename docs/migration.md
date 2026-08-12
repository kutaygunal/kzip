# Migration Guide: libzip (C) → kzip (Rust)

This guide helps existing libzip C consumers migrate to the Rust port. There are two
paths, depending on whether you can recompile:

- **C ABI migration:** link the `zip-sys` cdylib for the implemented subset of the
  `zip_*` C ABI. Check [C ABI / FFI status](ABI.md) before replacing a libzip build.
- **Idiomatic Rust:** use `zip-core` directly. This is the recommended path for new code.

The idiomatic API maps libzip's model onto Rust's ownership and `io` traits.

## Archive lifecycle

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_open(path, flags, &err)` | `Archive::open(File::open(path)?)?` |
| `zip_open` from a memory buffer | `Archive::open(Cursor::new(bytes))?` |
| `zip_get_num_entries` | `ar.len()` |
| `zip_close(z)` | drop the `Archive` |
| `zip_discard(z)` | drop without committing |

```rust
use std::fs::File;
use zip_core::Archive;

let ar = Archive::open(File::open("a.zip")?)?; // or Archive::open(Cursor::new(bytes))?
```

`Archive::open` takes any `Source` (`Read + Seek + Send + Sync`), so a `File`, a
`Cursor`, or a custom source all work.

## Reading entries

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_get_name` | `ar.name(index)` |
| `zip_fopen` / `zip_fopen_index` | `ar.open_entry(index)?` / `ar.open_by_name(name)?` |
| `zip_fread` | `io::Read` on the returned `EntryReader` |
| `zip_stat` | `ar.stat(index)?` |

```rust
use std::io::{self, Read};
use zip_core::Archive;

let ar = Archive::open(File::open("a.zip")?)?;
for i in 0..ar.len() {
    println!("entry {i}: {:?}", ar.name(i));
    let mut reader = ar.open_entry(i)?; // implements io::Read + io::Seek
    io::copy(&mut reader, &mut io::stdout())?;
}
```

## Writing (parallel)

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_source_buffer_create` + `zip_file_add` | `ArchiveFile::new(name, bytes)` |
| `zip_set_file_compression` | `CompressOptions { method, level, .. }` |
| `zip_close` (write) | `write_archive(&files, &opts)` |
| manual thread pool | `CompressOptions { parallel: true, workers }` (default) |

```rust
use zip_core::{write_archive, ArchiveFile, CompressOptions};

let files = vec![
    ArchiveFile::new("notes.txt", b"hello".to_vec()),
    ArchiveFile::new("photo.jpg", image_bytes),
];
let zip_bytes = write_archive(&files, &CompressOptions::default())?;
```

## In-place modify

kzip's true in-place path reuses compressed member bytes and rewrites only the central
directory + EOCD tail:

| libzip (C) | zip-core (Rust) |
|-------------|-----------------|
| `zip_rename` + `zip_close` (rewrites whole file) | `modify_archive(&bytes, renames, deletes)` |
| — | `modify_archive_file(path, renames, deletes)` (true in-place) |

```rust
use std::path::Path;
use zip_core::{modify_archive, modify_archive_file};

// Rename entry 0, delete entry 1 → new bytes in memory.
let patched = modify_archive(&zip_bytes, &[(0, "renamed.txt".to_string())], &[1])?;

// Same operation on disk, reusing existing compressed member data where possible.
modify_archive_file(Path::new("out.zip"), &[(0, "renamed.txt".to_string())], &[1])?;
```

## Error handling

libzip's two-axis error space (`zip_error_t` with a `zip_error_code` and a system `errno`)
maps to `zip_core::ZipError`, which carries a `ZipErrorCode` and, when applicable, a
system error. Prefer `Result` / `?` over checking error codes manually; the library is
panic-free on malformed input (see [Fuzzing & hardening](fuzzing.md)).

## What is *not* yet migrated

- **Async streaming:** use `zip-async` (tokio bridge) for non-blocking I/O.
- **C ABI coverage:** the `zip-sys` FFI layer covers the documented read/write surface;
  check [ABI.md](ABI.md) and the header for the exact symbol set and known gaps.
- **Codecs:** Zstd/LZMA are not yet enabled in the C baseline or the Rust codec layer
  (see the codec matrix in [Support matrix](support-matrix.md)); DEFLATE, Bzip2, and
  Store are available in both.

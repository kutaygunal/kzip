# C ABI status

`zip-sys` exposes a libzip-shaped C ABI backed by `zip-core`. It is a
compatibility subset, not the complete public API of C libzip. The committed
header at [`crates/zip-sys/include/zip.h`](../crates/zip-sys/include/zip.h) is
the source of truth for the supported C declarations.

## Build

```sh
cargo build --release -p zip-sys
```

The crate emits a shared library named `zip` (`zip.dll` on Windows and a
platform-equivalent shared object elsewhere) in `target/release/`. Include
`crates/zip-sys/include/zip.h` when compiling a C consumer. The consumer does
not need a Rust toolchain after the library has been built.

## Implemented areas

The current implementation covers these groups of the libzip model:

- archive open/close/discard, entry enumeration, name lookup, stat, and errors;
- entry reads, seeking, seekability, CRC/size validation, and encrypted reads;
- add, replace, delete, rename, directory creation, and close-time writes;
- archive/file comments, extra fields, timestamps, external attributes, and
  per-entry compression settings;
- stored, DEFLATE, and Bzip2 support as exposed by the method queries;
- ZipCrypto and WinZip AES-128/192/256 encryption;
- buffer, file, file-pointer, callback, window, layered, and archive-entry
  sources, including source read/write lifecycle helpers;
- `zip_open_from_source`, `zip_fdopen`, progress/cancel callbacks, archive
  flags, unchange operations, and the implemented Windows source helpers.

The Rust core also provides an idiomatic `Source` trait, deterministic optional
parallel compression, and async entry reads through `zip-async`. Those Rust APIs
are separate from the C ABI.

## Deliberate compatibility gaps

The following differences are known and should be checked before replacing an
existing libzip build:

- ZIP64 archives can be read, but the normal Rust writer does not yet cover the
  full ZIP64 write range.
- `zip_file_rename`, `zip_source_buffer_create`, and `zip_error_to_data` are not
  currently exported.
- Deprecated aliases `zip_add`, `zip_add_dir`, and `zip_replace` are not
  currently exported; use the modern operations in the supported surface.
- The Rust implementation and C libzip may differ in backend-specific details,
  codec availability, and unexposed flags. C callers must use the shipped
  header and not assume every upstream symbol is present.

## Verification

The differential harness compares the Rust library with the local C libzip
1.11.4 reference. It checks read/stat/error results and cross-reads archives
written by each implementation:

```sh
cargo test --workspace
bash run-verify.sh
```

On Windows, the wrapper requires a Bash environment that can find the Windows
Rust toolchain and the C reference DLL at `libs/c/zip.dll`; the same harness
binaries can be run directly from PowerShell when that setup is unavailable.

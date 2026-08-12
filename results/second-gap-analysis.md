# Second libzip C vs Rust gap analysis

Recorded: 2026-08-12  
Rust commit: `57cdea4`  
C reference: local libzip `1.11.4` (`libs/c/zip.dll`)

## Scope and method

This pass compares the current repository state against the local C reference,
not the earlier pre-implementation report.

- Upstream public function declarations were enumerated from
  `libzip/lib/zip.h`.
- Rust `zip-sys` `extern "C"` exports were enumerated from
  `crates/zip-sys/src/lib.rs`.
- Both DLLs were loaded with `ctypes`; every enumerated symbol was resolved.
- `zip_compression_method_supported` and
  `zip_encryption_method_supported` were queried for the relevant methods.
- The shipped `crates/zip-sys/include/zip.h` was compared with the upstream
  declaration set and constants.

## Results

### 1. Binary symbol parity: PASS

| Check | Result |
|---|---:|
| Upstream C public functions | 128 |
| Rust exported C-API functions | 128 |
| Rust-only helper exports | 3 |
| C DLL unresolved functions | 0 |
| Rust DLL unresolved functions | 0 |

The three Rust-only helpers are `zip_buffer_fragment`,
`zip_source_args_seek`, and `zip_source_window`. The six missing ABI symbols
from the first analysis are now present and resolve in the release DLL:
`zip_file_rename`, `zip_source_buffer_create`, `zip_error_to_data`,
`zip_add`, `zip_add_dir`, and `zip_replace`.

### 2. Shipped header completeness: OPEN

The binary ABI is complete for the enumerated C function set, but the shipped
header is not:

| Header item | Upstream | Shipped Rust header | Gap |
|---|---:|---:|---:|
| Public function declarations | 128 | 58 | 70 missing |
| `ZIP_*` macro/constants | 147 | 15 | 132 missing |

The missing function declarations are concentrated in:

- 43 `zip_source_*` lifecycle, callback, file, layered, and archive-entry
  source APIs;
- 21 metadata, archive-flag, extra-field, and unchange APIs;
- 6 open/error/callback APIs, including `zip_fdopen`, `zip_open_from_source`,
  progress/cancel registration, and `zip_error_get_sys_type`.

This means a C consumer including the committed `zip.h` cannot discover or
prototype many functions that the Rust DLL already exports. This is a header
contract gap, not a missing DLL implementation.

### 3. Compression capability parity: PARTIAL

The queried tuple is `(decompress, compress)`:

| Method | C libzip | Rust `zip-sys` | Result |
|---|---:|---:|---|
| Store (0) | `(1, 1)` | `(1, 1)` | match |
| DEFLATE (8) | `(1, 1)` | `(1, 1)` | match |
| Bzip2 (12) | `(1, 1)` | `(1, 0)` | Rust write gap |
| LZMA (14) | `(0, 0)` | `(0, 0)` | match; disabled baseline |
| LZMA2 (33) | `(0, 0)` | `(0, 0)` | match; disabled baseline |
| Zstandard (93) | `(0, 0)` | `(0, 0)` | match; disabled baseline |

The Rust core has a Bzip2 decoder but `compress_bytes` rejects Bzip2 for
writing. The C baseline was built with Bzip2 enabled and reports Bzip2 write
support. This is the highest-priority behavioral gap from this pass after the
header contract issue.

### 4. Encryption capability parity: PASS, signature audit OPEN

Both DLLs report support for ZipCrypto and AES-128/AES-192/AES-256, and reject
the unsupported AES method tested. Existing round-trip and cross-read tests
also cover the encrypted write/read paths.

The capability result does not mean the encryption entry point is a complete
drop-in ABI match. Upstream libzip 1.11.4 declares
`zip_file_set_encryption(zip_t *, zip_uint64_t, zip_uint16_t, const char *)`,
while the current Rust export and shipped header expose only the first three
arguments. C callers that pass a password directly through this entry point
must not treat the current signature as interchangeable with libzip.

## Remaining gap priorities

| Priority | Gap | Recommended next action |
|---|---|---|
| High | Shipped `zip.h` omits 70 exported function declarations and 132 upstream constants | Regenerate or systematically expand the header from the supported Rust ABI, then compile a C smoke test against every declaration |
| Medium | Bzip2 write support differs from the C baseline | Add a Rust Bzip2 encoder or make the C baseline disable Bzip2 compression and document the intentional mismatch |
| High | `zip_file_set_encryption` has a three-argument Rust/header signature versus libzip's four-argument declaration | Align the exported signature and password semantics, or explicitly expose this as a kzip-specific compatibility variant |
| Low | Full signature/layout audit for the expanded header | Compare typedefs, flags, structs, and calling conventions after header completion |

## Closed gaps confirmed

The first analysis gaps remain closed: ZIP64 writing, ZIP64 overflow extra
fields, ZIP64 EOCD records, AES data descriptors, the six missing ABI symbols,
and the associated regression tests.

# libzip C ABI — Rust export tracking

The Rust port (zip-sys) exports a **subset** of libzip's ~139-function public
API. This document tracks the status of every symbol we intend to support.

Legend:
- **COMPLETE** — implemented, tested, exported in the cdylib and in `zip.h`.
- **STUBBED** — exported with a placeholder that returns an error/0; not yet
  implemented.
- **DEFERRED** — not yet exported; planned for a later phase.

## Read path (Phase 4) — COMPLETE

| Symbol                | Status    | Notes |
|-----------------------|-----------|-------|
| `zip_open`            | COMPLETE  | read-only; `flags` ignored |
| `zip_close`           | COMPLETE  | frees the opaque handle |
| `zip_get_num_entries` | COMPLETE  | |
| `zip_get_name`        | COMPLETE  | valid until `zip_close` |
| `zip_fopen`           | COMPLETE  | open by name |
| `zip_fopen_index`     | COMPLETE  | open by index |
| `zip_fread`           | COMPLETE  | streaming `Read` |
| `zip_fclose`          | COMPLETE  | |
| `zip_stat`            | COMPLETE  | fill `zip_stat_t` by name |
| `zip_stat_index`      | COMPLETE  | fill `zip_stat_t` by index |
| `zip_stat_init`       | COMPLETE  | |
| `zip_strerror`        | COMPLETE  | |
| `zip_file_strerror`   | COMPLETE  | |
| `zip_libzip_version`  | COMPLETE  | returns `"1.11.4"` to match C baseline |
| `zip_name_locate`     | COMPLETE  | index of first entry named `name`, or -1 |

## Write / edit path — COMPLETE (buffer-source subset)

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_file_add` | COMPLETE | add entry from a buffer source; `ZIP_FL_OVERWRITE` replaces |
| `zip_dir_add` | COMPLETE | add a directory entry |
| `zip_delete` | COMPLETE | mark entry for deletion (applied on close) |
| `zip_rename` | COMPLETE | rename entry (applied on close) |
| `zip_file_replace` | COMPLETE | replace entry data (applied on close) |
| `zip_discard` | COMPLETE | free without writing |
| `zip_close` write-through | COMPLETE | materializes pending ops + writes on close |
| `zip_source_buffer` | COMPLETE | minimal buffer source (copies data) |
| `zip_source_free` | COMPLETE | release a buffer source |
| `zip_open` `ZIP_CREATE`/`ZIP_TRUNCATE`/`ZIP_RDONLY` | COMPLETE | create/truncate/read-only semantics |

> **Scope note:** the write path is driven by a minimal **buffer** source
> (`zip_source_buffer`). The full `zip_source_*` streaming API (file/function/
> layered/window/user-defined, 38 symbols) remains DEFERRED. `zip_add`/
> `zip_add_dir`/`zip_replace` (deprecated aliases) are not exported.

## Structured error object API — COMPLETE

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_get_error` | COMPLETE | pointer to the archive's `zip_error_t` |
| `zip_error_init` | COMPLETE | |
| `zip_error_init_with_code` | COMPLETE | |
| `zip_error_clear` | COMPLETE | |
| `zip_error_set` | COMPLETE | |
| `zip_error_strerror` | COMPLETE | |
| `zip_error_code_zip` | COMPLETE | |
| `zip_error_code_system` | COMPLETE | |
| `zip_error_fini` | COMPLETE | |
| `zip_error_to_str` | COMPLETE | deprecated helper |
| `zip_error_system_type` | COMPLETE | `ZIP_ET_NONE`/`ZIP_ET_SYS` |
| `zip_error_get` | COMPLETE | deprecated helper |
| `zip_error_set_from_source` | COMPLETE | resets to OK (sources carry no error) |

## fseek / ftell / seekability — COMPLETE

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_fseek` | COMPLETE | SEEK_SET/CUR/END on buffered entries |
| `zip_ftell` | COMPLETE | |
| `zip_file_is_seekable` | COMPLETE | always 1 (FFI serves entries as buffers) |

## Method-support queries — COMPLETE

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_compression_method_supported` | COMPLETE | Store/Deflate both ways; Bzip2 decompress-only |
| `zip_encryption_method_supported` | COMPLETE | `ZIP_EM_NONE` + `ZIP_EM_TRAD_PKWARE` |

## Comments & extra fields — READ COMPLETE, WRITE DEFERRED

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_get_archive_comment` | COMPLETE | read |
| `zip_file_get_comment` | COMPLETE | read |
| `zip_file_extra_fields_count` | COMPLETE | read |
| `zip_file_extra_fields_count_by_id` | COMPLETE | read |
| `zip_file_extra_field_get` | COMPLETE | read |
| `zip_file_extra_field_get_by_id` | COMPLETE | read |
| `zip_set_archive_comment`, `zip_file_set_comment`, `zip_file_extra_field_set`/`delete` | DEFERRED | write side needs writer comment/extra-field support |

## Encryption — ZipCrypto (traditional PKWARE) COMPLETE, WinZip AES DEFERRED

Phase 1 adds traditional PKWARE (ZipCrypto) read + write:
`zip_fopen_encrypted`, `zip_fopen_index_encrypted`, `zip_set_default_password`,
`zip_file_set_encryption` (method `ZIP_EM_TRAD_PKWARE` = 1). Encrypted entries
are decrypted on read with the correct password (wrong password →
`ZIP_ER_WRONGPASS`, none → `ZIP_ER_NOPASS`); `zip_stat` reports
`ZIP_EM_TRAD_PKWARE`. WinZip AES (methods 2-3) remains deferred.

## Deferred (dedicated phases)

- **Full `zip_source_*` streaming source API** (38 symbols: buffer/file/function/
  layered/window/user-defined) — only the minimal buffer source is exported.
- **Encryption: WinZip AES** (read & write) — ZipCrypto (traditional PKWARE) is
  done; AES-128/192/256 remains for Phase 2.
- **Progress/cancel callbacks**, Win32 sources, `zip_fdopen`/`zip_open_from_source`,
  `zip_unchange*`, `zip_register_progress_callback*`,
  `zip_register_cancel_callback_with_state`.
- Deprecated aliases (`zip_add`, `zip_add_dir`, `zip_replace`, `zip_rename`
  is the modern name and IS exported).

## Thread safety

libzip's `zip_t`/`zip_file_t` are **not** thread-safe. The Rust FFI goes further
and is safe for concurrent **read** access to a *single* shared handle:

- Handles are stored behind `Box` and recovered with **shared** `&` references
  (`as_ref()`), never an aliasing `&mut` from the raw pointer. All mutation goes
  through interior mutability (`Mutex` / `AtomicI32`), so concurrent `zip_fopen`
  / `zip_fread` on the same handle are serialized and data-race-free.
- `zip_open` reads the file into an in-memory contiguous buffer source, so the
  archive's per-entry `duplicate()` is a pure clone (no shared OS file-pointer).
  This is covered by `crates/zip-sys/src/lib.rs` concurrency tests
  (`ffi_shared_archive_handle_concurrent_fopen`,
  `ffi_shared_file_handle_concurrent_fread`).
- `zip_close` / `zip_fclose` take exclusive ownership (`Box::from_raw`) and must
  not race any other operation on the same handle — matching the libzip
  ownership contract.

## Generated header

`crates/zip-sys/include/zip.h` mirrors the COMPLETE subset. Regenerate with
`scripts/gen-zip-h.sh` (requires `cargo install cbindgen`); otherwise the header
is the committed source of truth.

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

## Write / edit path — DEFERRED

| Symbol | Status | Notes |
|--------|--------|-------|
| `zip_add`, `zip_add_dir`, `zip_delete`, `zip_rename`, `zip_replace` | DEFERRED | needs the write engine (zip-core Phase 2) |
| `zip_close` write-through, `zip_discard` | DEFERRED | |
| `zip_source_buffer`, `zip_source_file`, `zip_source_filep` | DEFERRED | source-construction API |
| `zip_set_default_password`, `zip_file_add` | DEFERRED | |
| `zip_error_*`, `zip_register_progress_callback`, `zip_register_cancel_callback` | DEFERRED | |

## Encryption — DEFERRED

`zip_fopen_encrypted`, `zip_fopen_index_encrypted`, `zip_set_encryption`,
`zip_file_set_encryption` are deferred (zip-core encryption lands in a later
phase; the read path currently rejects encrypted entries with
`ZIP_ER_ENCRNOTSUPP`).

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

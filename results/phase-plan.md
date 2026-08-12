# kzip — Gap-Closure Phase Plan (Prioritized)

**Planner:** `planner` (w3:p2)
**Source of truth:** `results/verification-report.md` (§4.1 functional-gap table, §5 prioritized gaps)
**Cross-checked against:** `results/verification-report.md`, `git log`, current `crates/*` source
**Date:** 2026

---

## 0. Current-state reconciliation (READ FIRST)

The `verification-report.md` is dated 2024-08-04 and predates four commits that
have **already closed** most of its §5 table. The scrum-master must NOT re-assign
these. Evidence is in `git log` and `results/verification-report.md`.

| §5 item | Gap | Status | Evidence |
|---|---|---|---|
| 1 (Critical) | ZIP64 EOCD field-offset bug | **CLOSED** | `29ddb0c`; `cdir.rs::read_eocd64` reads `[32:40]`/`[40:48]`/`[48:56]` correctly; `many_zip64.zip` (70k) opens in both libs (migration chunk 1). |
| 2 (High) | `valid` bitmask `0xFF` vs `0xFFFFFFFF` | **CLOSED** | `archive.rs::stat` sets `valid: 0xFF`; test `stat_valid_and_encryption_match_libzip`. |
| 3 (High) | unencrypted `encryption_method` → `ZIP_EM_NONE`(0) | **CLOSED** | `dirent.rs` maps unencrypted → `constant::encryption::NONE`; test asserts `Some(0)`. |
| 4 (High) | timezone-agnostic `mtime` | **CLOSED** | `archive.rs::dos_to_unix` mirrors `mktime`/`tm_isdst=-1`; tests `dos_to_unix_uses_local_timezone_like_mktime` etc. |
| 5 (High) | missing `ZIP_ER_TRUNCATED_ZIP`(35)/`EF_TOO_LARGE`(36) | **CLOSED** | `error.rs` has `TruncatedZip=35`, `Eftoolarge=36`; truncated → 35 (migration chunk 3). |
| 6 (Medium) | `zip_stat_t` missing trailing `flags` | **CLOSED** | `zip-sys` `zip_stat` has `flags: u32` (60-byte layout, migration chunk 3). |
| 9 (Medium) | error-string capitalization | **CLOSED** | `36dcc81`; `zip-sys::err_str` now byte-matches libzip `_zip_err_str[]`. |
| — (security) | zip-bomb: unbounded decompress / CD alloc | **CLOSED** | `a2c4973`; `MAX_DECOMPRESSED`, `MAX_CD_BUFFER`/`MAX_CD_SIZE` caps + tests. |
| — (security) | mutex-poison panic, unbounded `zip_open` read | **CLOSED** | `36dcc81`; `guarded()` catch_unwind, bounded read. |
| 8 (Medium) | write/edit export through `zip-sys` | **PARTIAL** | `ee3b693` exported `zip_file_add`, `zip_dir_add`, `zip_delete`, `zip_rename`, `zip_file_replace`, `zip_discard`, `zip_source_buffer/free`. Remaining write-metadata APIs are in Phase 3. |

**Net remaining work:** all §5 items still open are **Medium** (item 7
encryption, remainder of item 8) and **Low–Med** (item 10). There are **no
remaining Critical or High items** — the plan therefore starts at Medium and
descends to Low–Med, grouping related work into session-sized phases.

---

## 1. Phase list (summary)

| Phase | Title | Priority | Effort |
|---|---|---|---|
| 1 | Encryption: ZipCrypto (PKWARE) read + write | Medium | Medium |
| 2 | Encryption: WinZip AES read + write | Medium | Medium |
| 3 | Write-path metadata: comments, extra fields, mtime, attributes, compression | Medium | Medium |
| 4 | Streaming `zip_source_*` core (file/function/layered/window/zip) | Low–Med | Large (split) |
| 5 | `zip_open_from_source` / `zip_fdopen` + write-mode sources | Low–Med | Medium |
| 6 | Progress & cancel callbacks | Low–Med | Medium |
| 7 | Archive flags, `unchange*`, method-query, file-error APIs | Low–Med | Small–Med |
| 8 | Win32 sources + source utility helpers | Low–Med | Medium |

---

## 2. Detailed phases

### Phase 1 — Encryption: ZipCrypto (PKWARE) read + write
**Priority:** Medium · **Effort:** Medium · **§5 item:** 7

**Files/modules to touch**
- `crates/zip-core/src/crypto.rs` (NEW — ZipCrypto keystream, ~150 LOC)
- `crates/zip-core/src/constant.rs` (encryption-method constants already present)
- `crates/zip-core/src/archive.rs` (open encrypted entry, password plumbing)
- `crates/zip-core/src/reader.rs` (decrypt-on-read layer)
- `crates/zip-core/src/compress.rs` (encrypt-on-write layer)
- `crates/zip-core/src/dirent.rs` (surface `ZIP_EM_TRAD_PKWARE`)
- `crates/zip-sys/src/lib.rs` (new `#[no_mangle]` exports)

**New C ABI exports:** `zip_fopen_encrypted`, `zip_fopen_index_encrypted`,
`zip_file_set_encryption`, `zip_set_default_password`.

**Acceptance criteria**
- A ZipCrypto-encrypted archive written by C libzip reads **byte-identically**
  in `zip-core`/`zip-sys` (differential harness, `verify_read`).
- A Rust-written ZipCrypto archive reads **byte-identically** in C libzip
  (cross-read harness).
- Wrong password → `ZIP_ER_WRONGPASS`(27); no password → `ZIP_ER_NOPASS`(26);
  `zip_strerror` strings match C.
- `zip_file_set_encryption` + `zip_set_default_password` round-trip through the
  C ABI; `zip_stat` reports `ZIP_EM_TRAD_PKWARE` for encrypted entries.
- No panic on malformed/truncated encrypted input (fuzz/robustness tests).

---

### Phase 2 — Encryption: WinZip AES read + write
**Priority:** Medium · **Effort:** Medium · **§5 item:** 7

**Files/modules to touch**
- `crates/zip-core/src/crypto.rs` (AES-128/192/256, PBKDF2-HMAC-SHA1 key
  derivation, HMAC-SHA1 integrity)
- `crates/zip-core/src/archive.rs`, `reader.rs`, `compress.rs`, `dirent.rs`
- `crates/zip-core/Cargo.toml` (add `aes`, `cbc`, `ctr`, `hmac`, `sha1`,
  `pbkdf2` — RustCrypto, per PLAN.md §4.2)
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_encryption_method_supported`.

**Acceptance criteria**
- AES-128/256 archives written by C libzip read **byte-identically** in Rust;
  Rust-written AES archives read **byte-identically** in C (both directions).
- HMAC-SHA1 integrity check: corrupted ciphertext → `ZIP_ER_CRC`/integrity error.
- Wrong password → `ZIP_ER_WRONGPASS`; `zip_encryption_method_supported`
  returns true for AES methods, false for unsupported.
- `zip_stat` reports `ZIP_EM_AES_128/192/256` correctly.
- No panic on malformed input.

---

### Phase 3 — Write-path metadata: comments, extra fields, mtime, attributes, compression
**Priority:** Medium · **Effort:** Medium · **§5 items:** 8, 10

**Files/modules to touch**
- `crates/zip-core/src/compress.rs` (writer: comments, extra fields, mtime,
  attributes, per-entry compression)
- `crates/zip-core/src/dirent.rs` (serialize/parse comment + extra fields)
- `crates/zip-core/src/archive.rs` (setters)
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_file_set_comment`, `zip_set_file_comment`,
`zip_set_archive_comment`, `zip_get_file_comment`, `zip_file_extra_field_set`,
`zip_file_extra_field_delete`, `zip_file_extra_field_delete_by_id`,
`zip_file_set_mtime`, `zip_file_set_dostime`, `zip_file_set_external_attributes`,
`zip_file_get_external_attributes`, `zip_file_attributes_init`,
`zip_set_file_compression`.

**Acceptance criteria**
- Write→read round-trip preserves archive/file comments, extra fields, mtime,
  and external attributes exactly.
- C libzip reads Rust-written metadata **identically** (cross-read harness);
  Rust reads C-written metadata identically.
- `zip_set_file_compression` selects Store/Deflate per entry; output remains
  byte-identical to C for the same settings.
- `zip_file_extra_field_set`/`delete` round-trip; counts via existing
  `zip_file_extra_fields_count*` reflect changes.

---

### Phase 4 — Streaming `zip_source_*` core (file/function/layered/window/zip)
**Priority:** Low–Med · **Effort:** Large (split into 4a/4b if needed) · **§5 item:** 10

**Files/modules to touch**
- `crates/zip-core/src/source.rs` (extend the internal `Source` trait into the
  public layered-source model; PLAN.md §5.1 "crown jewel")
- `crates/zip-core/src/archive.rs` (open from a source)
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_source_file`, `zip_source_file_create`,
`zip_source_filep`, `zip_source_filep_create`, `zip_source_function`,
`zip_source_function_create`, `zip_source_layered`, `zip_source_layered_create`,
`zip_source_window_create`, `zip_source_zip`, `zip_source_zip_create`,
`zip_source_zip_file`, `zip_source_zip_file_create`, `zip_source_read`,
`zip_source_write`, `zip_source_open`, `zip_source_close`, `zip_source_seek`,
`zip_source_seek_write`, `zip_source_stat`, `zip_source_at_eof`,
`zip_source_is_seekable`, `zip_source_keep`, `zip_source_error`,
`zip_source_is_deleted`, `zip_source_get_file_attributes`.

**Acceptance criteria**
- A C consumer can build a layered pipeline
  `file → window → crc → compress` and read/write through it; results match C
  libzip byte-for-byte.
- `zip_source_function` user callbacks (open/read/close/seek/stat) are invoked
  with correct command semantics.
- `zip_source_zip` reads an entry from another archive as a source.
- `zip_source_read`/`write`/`seek`/`stat`/`at_eof`/`is_seekable` return values
  match C for identical inputs.
- No panic on malformed source callbacks.

---

### Phase 5 — `zip_open_from_source` / `zip_fdopen` + write-mode sources
**Priority:** Low–Med · **Effort:** Medium · **§5 item:** 10

**Files/modules to touch**
- `crates/zip-core/src/archive.rs`, `source.rs`
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_open_from_source`, `zip_fdopen`,
`zip_source_begin_write`, `zip_source_begin_write_cloning`,
`zip_source_commit_write`, `zip_source_rollback_write`,
`zip_source_make_command_bitmap`, `zip_source_pass_to_lower_layer`,
`zip_source_seek_compute_offset`, `zip_source_args_seek`.

**Acceptance criteria**
- `zip_open_from_source` opens an archive from a buffer/file source; error codes
  match C (NOZIP/TRUNCATED/INCONS).
- `zip_fdopen` wraps a file descriptor; reads match C.
- Write-mode source lifecycle (begin_write → write → commit/rollback) behaves
  like C; rollback discards, commit persists.
- Utility helpers return values identical to C.

---

### Phase 6 — Progress & cancel callbacks
**Priority:** Low–Med · **Effort:** Medium · **§5 item:** 10

**Files/modules to touch**
- `crates/zip-core/src/compress.rs` (cooperative poll points; PLAN.md §5.1)
- `crates/zip-core/src/archive.rs`
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_register_progress_callback`,
`zip_register_progress_callback_with_state`,
`zip_register_cancel_callback_with_state`.

**Acceptance criteria**
- Progress callback fires with monotonically increasing `zip_progress_t` during
  compression; final value reaches 1.0.
- Cancel callback returning non-zero aborts the operation → `ZIP_ER_OPNOTSUPP`
  or `ZIP_ER_CANCELLED`(32); no partial/corrupt output committed.
- Callbacks with user state (`_with_state`) receive the correct state pointer.
- Deterministic output unchanged when callbacks are registered (no behavior
  change to bytes).

---

### Phase 7 — Archive flags, `unchange*`, method-query, file-error APIs
**Priority:** Low–Med · **Effort:** Small–Med · **§5 item:** 10

**Files/modules to touch**
- `crates/zip-core/src/archive.rs`, `error.rs`
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_get_archive_flag`, `zip_set_archive_flag`,
`zip_unchange`, `zip_unchange_all`, `zip_unchange_archive`,
`zip_compression_method_supported`, `zip_get_num_files` (alias),
`zip_file_error_clear`, `zip_file_error_get`, `zip_file_get_error`,
`zip_error_get_sys_type`.

**Acceptance criteria**
- `zip_set_archive_flag`/`get_archive_flag` round-trip; flags match C.
- `zip_unchange*` reverts pending edits (add/delete/rename) to the on-disk
  state; `zip_unchange_all` resets the whole archive.
- `zip_compression_method_supported` returns correct bool per method.
- `zip_file_error_clear`/`get`/`get_error` return correct codes/strings.
- `zip_error_get_sys_type` returns the correct system-error type.

---

### Phase 8 — Win32 sources + source utility helpers
**Priority:** Low–Med · **Effort:** Medium · **§5 item:** 10

**Files/modules to touch**
- `crates/zip-core/src/source.rs`
- `crates/zip-sys/src/lib.rs`

**New C ABI exports:** `zip_source_win32a`, `zip_source_win32a_create`,
`zip_source_win32w`, `zip_source_win32w_create`, `zip_source_win32handle`,
`zip_source_win32handle_create`, `zip_buffer_fragment`.

**Acceptance criteria**
- Win32 ANSI/wide/handle sources open and read archives identically to the
  stdio file source.
- `zip_buffer_fragment` accepts a fragment array and reads correctly.
- All helpers return values identical to C; no panic on invalid handles.

---

## 3. Notes for scrum-master / orchestrator

1. **Do not re-assign §5 items 1–6, 9** — they are closed (see §0). Start the
   loop at **Phase 1**.
2. **Phase 4 is the largest**; if a single session cannot finish it, split into
   4a (read-side sources: file/function/window/zip + read/seek/stat) and 4b
   (write-side + layered). The scrum-master should write tests for 4a first.
3. **Dependency order:** Phase 1 and 2 (encryption) are independent of 3–8 and
   can be parallelized across engineers if the orchestrator wants throughput;
   Phases 4→5 are sequential (5 builds on 4's source model).
4. **Testing harness:** reuse `differential/src/bin/{verify_read,cross_read}.rs`
   and `run-verify.sh`; extend them per phase. The scrum-master writes the
   phase test cases before assigning to the engineer.
5. **Constraint:** `zip-core` core logic may be modified only where a phase
   explicitly requires it (encryption, sources, metadata writer). Read-path
   equivalence already proven must not regress — re-run `run-verify.sh` after
   every phase.
6. **Effort legend:** Small ≈ <1 session; Medium ≈ 1 session; Large ≈ 1–2
   sessions (split if needed). All phases are sized for a single engineer.

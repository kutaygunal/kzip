# Phase 7 — Archive flags, `unchange*`, method-query, file-error APIs — Test Cases

**Phase:** 7 · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 7.
**Depends on:** Phases 1–6. Phase 6 — DONE (`afecbb6`).
**Constraint:** `zip-core` core logic may be modified only where Phase 7 requires
it. Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Current-state reconciliation (READ FIRST)

- **Already exported — do not re-assign:**
  - `zip_compression_method_supported` (present).
  - Structured error infra (present): `zip_get_error`, `zip_error_clear`,
    `zip_error_strerror`, `zip_error_code_zip`, `zip_error_code_system`,
    `zip_error_system_type`, `zip_error_get`.
  - `zip_get_num_entries` exists (the `zip_get_num_files` alias may be added, but
    the read-side function is present).
- **MISSING — Phase 7 deliverables:**
  - `zip_get_archive_flag`, `zip_set_archive_flag`
  - `zip_unchange`, `zip_unchange_all`, `zip_unchange_archive`
  - `zip_file_error_clear`, `zip_file_error_get`, `zip_file_get_error`
  - `zip_error_get_sys_type`
  - `zip_get_num_files` (alias for `zip_get_num_entries`), if required by C
    consumers.
- Reference error codes present in `zip-core`: `ZIP_ER_OPNOTSUPP`=28,
  `ZIP_ER_CANCELLED`=32, plus the full libzip range.

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass. Any pre-existing failure here
means STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 7):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The read
path must remain byte-identical.

---

## 2. TC-1 — `zip_set_archive_flag` / `zip_get_archive_flag` round-trip

**Command:**
```bash
cargo test --package zip-sys archive_flag_round_trip
```

**Expected:**
- `zip_set_archive_flag(zh, ZIP_AFL_*, value, 0)` then `zip_get_archive_flag(...)`
  returns the same value — matches C libzip for the same flag bits.
- Both `ZIP_AFL_RDONLY` and `ZIP_AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE`
  round-trip correctly.
- Setting an invalid flag id returns `-1` and sets a defined error (no panic).

---

## 3. TC-2 — `zip_unchange` / `zip_unchange_all` / `zip_unchange_archive` revert pending edits

**Command:**
```bash
cargo test --package zip-sys unchange_revert
```

**Expected:**
- **`zip_unchange(zh, index)`** reverts a single entry's pending changes
  (e.g. a pending `zip_rename`/`zip_delete`/`zip_file_replace` on that entry)
  back to the on-disk state; after reopen the entry reflects the disk, not the
  edit.
- **`zip_unchange_all(zh)`** reverts all per-entry pending edits (adds, renames,
  deletes, replaces) for the whole archive back to the on-disk state.
- **`zip_unchange_archive(zh)`** reverts archive-level pending changes (e.g. a
  pending `zip_set_archive_comment`, archive flags) to the on-disk state, without
  touching entry-level edits.
- After each `unchange*`, a subsequent close/reopen yields bytes identical to the
  original on-disk archive (no residual edit).

---

## 4. TC-3 — `zip_compression_method_supported` returns correct bool per method

**Command:**
```bash
cargo test --package zip-sys compression_method_supported
```
(extend the existing `zip_compression_method_supported` test.)

**Expected:**
- `zip_compression_method_supported(ZIP_CM_STORE, 0/1)` → true for both
  compress/decompress.
- `zip_compression_method_supported(ZIP_CM_DEFLATE, 0/1)` → true.
- `ZIP_CM_BZIP2` and `ZIP_CM_LZMA` → match C libzip's supported set for this
  build (verify against C, do not hard-code a guess).
- An unknown/unsupported method id → `0` (false).

---

## 5. TC-4 — `zip_file_error_clear` / `zip_file_error_get` / `zip_file_get_error` return correct codes/strings

**Command:**
```bash
cargo test --package zip-sys file_error_apis
```

**Expected:**
- Trigger a file error (e.g. `zip_fopen` on a missing entry → `ZIP_ER_NOENT`=9,
  or a read/CRC failure).
- `zip_file_error_get(fh, &zep, &sep)` returns the zip error code and system error
  code in the out-params (matches C).
- `zip_file_get_error(fh)` returns a `zip_error_t*` whose
  `zip_error_code_zip`/`zip_error_code_system`/`zip_error_strerror` match.
- `zip_file_error_clear(fh)` resets the file error to no-error; subsequent
  queries report success.
- Codes/strings match C libzip exactly for identical inputs.

---

## 6. TC-5 — `zip_error_get_sys_type` returns the correct system-error type

**Command:**
```bash
cargo test --package zip-sys error_get_sys_type
```

**Expected:**
- After triggering a known system error (e.g. an open/read failure),
  `zip_error_get_sys_type(ze)` returns the correct `zip_error_type_t`
  (e.g. `ZIP_ET_SYS` for OS errors, `ZIP_ET_ZLIB`/`ZIP_ET_NONE` as applicable),
  matching C libzip for the same underlying error.
- For a pure zip-code error with no system component, it returns the appropriate
  non-`SYS` type.
- No panic on a NULL/invalid error pointer (defined error instead).

---

## 7. TC-6 — No panic on malformed input

**Command:**
```bash
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_entry_reader
```
plus robustness unit tests:
```bash
cargo test --package zip-sys phase7_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: `unchange` on an
out-of-range index, archive-flag set on a closed/invalid archive, error APIs with
NULL out-params, and `zip_error_get_sys_type` on a NULL/invalid error. Malformed
inputs yield a defined error code, never a panic.

---

## 8. New C ABI symbols required (engineer must export)

`zip_get_archive_flag`, `zip_set_archive_flag`, `zip_unchange`,
`zip_unchange_all`, `zip_unchange_archive`, `zip_file_error_clear`,
`zip_file_error_get`, `zip_file_get_error`, `zip_error_get_sys_type`,
`zip_get_num_files` (alias), and re-verify `zip_compression_method_supported`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all new symbols resolve via `libloading`/`dlopen`; the pre-existing
structured-error and `zip_compression_method_supported` symbols still resolve.

---

## 9. Pass criteria (summary)

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-1: `zip_set_archive_flag`/`get_archive_flag` round-trip; flags match C.
4. TC-2: `zip_unchange`/`unchange_all`/`unchange_archive` revert pending edits to
   the on-disk state.
5. TC-3: `zip_compression_method_supported` returns correct bool per method.
6. TC-4: `zip_file_error_clear`/`get`/`get_error` return correct codes/strings.
7. TC-5: `zip_error_get_sys_type` returns the correct system-error type.
8. TC-6: no panic on malformed input (fuzz + unit).
9. All new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

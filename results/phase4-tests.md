# Phase 4 — Streaming `zip_source_*` core — Test Cases

**Phase:** 4 · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer-4` (parallel agent) · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 4 and §3.2.
**Depends on:** Phases 1–3 (encryption, metadata). Phase 3 is in parallel; if
Phase 3 is not yet merged when 4a lands, 4a tests must not depend on Phase-3
symbols.

---

## 0. Split note (plan §3.2)

Phase 4 is the **largest** phase. Per the plan, it is split:

- **4a (read-side sources)** — `file` / `function` / `window` / `zip` sources +
  `read` / `seek` / `stat` / `at_eof` / `is_seekable`. **This document tests 4a.**
- **4b (write-side + layered)** — write-mode sources and the layered pipeline
  `file → window → crc → compress`. A separate `results/phase4b-tests.md` will be
  written when 4a lands.

This document is therefore scoped to **4a** (read-side streaming). Items in the
plan that are purely write/layered (e.g. `zip_source_write`/`commit_write`) are
deferred to 4b and are listed only as a forward note (§9).

---

## 1. Current-state reconciliation (READ FIRST)

- `zip-core` already has an **internal** `Source` trait (`crates/zip-core/src/
  source.rs`, `trait Source: Read + Seek + Send + Sync`) with impls for
  `std::fs::File`, `Cursor<Vec<u8>>`, `Cursor<Box<[u8]>>`, plus
  `Supports` capability flags (`has_read`/`has_seek`/`has_write`) and a `Stat`
  struct. Phase 4 exposes this into the public layered `zip_source_*` model.
- `zip-sys` already exports `zip_source_buffer` (line ~882) and
  `zip_source_free` (line ~914) — **do not re-assign these**.
- The **read-side source symbols below are the 4a deliverable** and are MISSING.

---

## 2. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass. Any pre-existing failure here
means STOP and report to the engineer.

**Regression gate (must stay PASS after 4a):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The whole
read path must remain byte-identical.

---

## 3. TC-4a-1 — `zip_source_file` / `zip_source_filep` open + read a real archive

**Command:**
```bash
cargo test --package zip-sys source_file_read
```

**Expected:**
- `zip_source_file_create(path, 0, -1, &err)` (or `zip_source_filep_create`)
  creates a seekable, readable source over an on-disk `.zip`.
- `zip_open_from_source` (or `zip_open` on the source) opens the archive and
  every entry reads **byte-identically** to the same archive opened directly
  (FNV/SHA-256 compare against ground truth).
- `zip_source_is_seekable(src) == 1`, `zip_source_at_eof(src) == 0` before read,
  `1` after exhausting.
- `zip_source_stat(src)` returns the file size.

---

## 4. TC-4a-2 — `zip_source_function` callbacks invoked with correct commands

**Setup:** a Rust-side user callback implementing the `zip_source_callback` ABI
(`ZIP_SOURCE_OPEN/READ/CLOSE/SEEK/STAT/TELL/SUPPORTS/ERROR`) over a fixed byte
slice.

**Command:**
```bash
cargo test --package zip-sys source_function_callbacks
```

**Expected:**
- `zip_source_function_create(cb, ud, &err)` succeeds and the callback is
  invoked.
- The command sequence matches libzip semantics: `OPEN` before first `READ`;
  `STAT` returns correct size; `SEEK`/`TELL` report/seek correctly;
  `SUPPORTS` returns the readable+seekable bitmask; `READ` returns the exact
  requested bytes or `0` at EOF.
- Reading an archive built on this function source yields byte-identical entries
  to the source buffer equivalent.
- `CLOSE` is called exactly once; `ERROR` is reachable on a failing source.

---

## 5. TC-4a-3 — `zip_source_window` reads a sub-range

**Command:**
```bash
cargo test --package zip-sys source_window_read
```

**Expected:**
- `zip_source_window_create(src, offset, len, &err)` yields a source exposing
  exactly `len` bytes starting at `offset`.
- Reading the window returns exactly those bytes; `zip_source_at_eof` becomes `1`
  after the last byte; seeking within the window behaves like C libzip (bounds
  relative to the window, not the underlying source).
- A window over a valid archive's byte range can itself be opened as an archive.

---

## 6. TC-4a-4 — `zip_source_zip` reads an entry from another archive as a source

**Command:**
```bash
cargo test --package zip-sys source_zip_entry
```

**Expected:**
- `zip_source_zip_create(src_archive, index, flags, 0, -1, &err)` produces a
  source over that entry's (decompressed) bytes.
- Adding that source into a second archive (via `zip_file_add`) copies the entry
  content correctly; the copied entry's bytes match the original entry
  (SHA-256/byte compare).
- Behavior matches C libzip for `flags` (e.g. `ZIP_FL_COMPRESSED` raw vs
  decompressed) and for the start/len sub-range arguments.
- Reading a whole archive whose entry came from `zip_source_zip` is
  byte-identical.

---

## 7. TC-4a-5 — `read` / `seek` / `stat` / `at_eof` / `is_seekable` match C

**Command:**
```bash
cargo test --package zip-sys source_read_seek_stat_parity
```
plus a differential probe against real C libzip:
```bash
bash run-verify.sh   # harness extended to fingerprint source primitives
```

**Expected:** for identical inputs, return values match C libzip:
- `zip_source_read` returns the requested count or bytes actually read; `0` at EOF.
- `zip_source_seek` (SEEK_SET/CUR/END) offsets and `zip_source_seek`/`tell`
  return correct absolute positions.
- `zip_source_stat` returns matching size/mtime/type.
- `zip_source_at_eof` and `zip_source_is_seekable` match C for file (seekable),
  window (seekable), and function (per `SUPPORTS`).

---

## 8. TC-4a-6 — No panic on malformed source callbacks

**Command:**
```bash
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_entry_reader
```
plus robustness unit tests:
```bash
cargo test --package zip-sys source_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: a user callback
returning invalid command IDs, a callback returning `-1`/errno without setting a
source error, a callback over a truncated buffer, a window with `len` exceeding
the underlying source, and a `zip_source_zip` over a deleted entry. Malformed
inputs yield a defined error (e.g. `ZIP_ER_READ`, `ZIP_ER_INCONS`), never a
panic.

---

## 9. New C ABI symbols required for 4a (engineer must export)

`zip_source_file`, `zip_source_file_create`, `zip_source_filep`,
`zip_source_filep_create`, `zip_source_function`, `zip_source_function_create`,
`zip_source_window_create`, `zip_source_zip`, `zip_source_zip_create`,
`zip_source_zip_file`, `zip_source_zip_file_create`, `zip_source_read`,
`zip_source_open`, `zip_source_close`, `zip_source_seek`, `zip_source_stat`,
`zip_source_at_eof`, `zip_source_is_seekable`, `zip_source_error`,
`zip_source_is_deleted`, `zip_source_keep`, `zip_source_get_file_attributes`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all 4a symbols resolve via `libloading`/`dlopen`.

---

## 10. Forward note (4b, not tested here)

Deferred to a later `results/phase4b-tests.md` when 4a lands:
- `zip_source_layered*`, `zip_source_write`, `zip_source_seek_write`,
  `zip_source_begin_write*`, `zip_source_commit_write`, `zip_source_rollback_write`.
- The layered pipeline `file → window → crc → compress` read+write byte-identical
  to C.
- `zip_source_function` write-mode commands.

---

## 11. Pass criteria (summary) — Phase 4a

All of the following must hold to hand off to `devops` (4a only):

1. `cargo test --workspace` green (no regressions).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-4a-1: file/filep source opens + reads archive byte-identically.
4. TC-4a-2: function source callbacks invoked with correct command semantics.
5. TC-4a-3: window source reads sub-range exactly.
6. TC-4a-4: `zip_source_zip` reads an entry from another archive as a source.
7. TC-4a-5: read/seek/stat/at_eof/is_seekable match C.
8. TC-4a-6: no panic on malformed source callbacks (fuzz + unit).
9. All 4a C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer-4` (orchestration
loop step 5).

# Phase 4b — Streaming `zip_source_*` write-side + layered sources — Test Cases

**Phase:** 4b · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer-4b` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 4 (write-side
+ layered) and §3.2; extends `results/phase4-tests.md` (4a, read-side).
**Depends on:** Phase 4a (read-side sources) — DONE, committed `f6eb17c`.
**Constraint:** `zip-core` core logic may be modified only where 4b requires it.
Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Split note (plan §3.2)

Phase 4 is split into:
- **4a (read-side sources)** — file/function/window/zip + read/seek/stat/at_eof/
  is_seekable — **DONE (`f6eb17c`)**, covered by `results/phase4-tests.md`.
- **4b (write-side + layered)** — this document. It covers `zip_source_layered*`,
  `zip_source_write`/`seek_write`, write-mode lifecycle
  (`begin_write` → `commit_write`/`rollback_write`), and the full
  `file → window → crc → compress` pipeline producing **byte-identical output to C**.

---

## 1. Current-state reconciliation (READ FIRST)

- All **4a read-side source symbols are already exported** in `zip-sys`
  (`zip_source_file*`, `zip_source_function*`, `zip_source_window_create`,
  `zip_source_zip*`, `zip_source_read/seek/tell/stat/at_eof/is_seekable/open/
  close/keep/error/is_deleted/get_file_attributes`) — **do not re-assign**.
- The **4b symbols below are MISSING** and are this phase's deliverable:
  `zip_source_layered`, `zip_source_layered_create`, `zip_source_write`,
  `zip_source_seek_write`, `zip_source_begin_write`,
  `zip_source_begin_write_cloning`, `zip_source_commit_write`,
  `zip_source_rollback_write`, `zip_source_make_command_bitmap`,
  `zip_source_pass_to_lower_layer`, `zip_source_seek_compute_offset`,
  `zip_source_args_seek`.
- `zip-core` has the internal `Source` trait and `Supports` capability flags
  (incl. `Writable`) plus a 4a layered model to build on.

---

## 2. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass (incl. 4a read-side source
tests). Any pre-existing failure here means STOP and report to the engineer.

**Regression gate (must stay PASS after 4b):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The read
path must remain byte-identical.

---

## 3. TC-4b-1 — `zip_source_layered` / `zip_source_layered_create` wrap a source

**Command:**
```bash
cargo test --package zip-sys source_layered_wrap
```

**Expected:**
- `zip_source_layered_create(lower, cb, ud, &err)` wraps an existing lower
  source and returns a valid layered source.
- Reads/seeks/stat/at_eof propagate correctly from the upper layer to the lower
  layer; results match C libzip for identical inputs.
- `zip_source_layered` (the "open" variant) attaches the layer and behaves
  identically.
- The layered source is seekable/readable according to its `SUPPORTS`/lower
  source capabilities.

---

## 4. TC-4b-2 — `zip_source_write` / `zip_source_seek_write` write-mode behavior

**Command:**
```bash
cargo test --package zip-sys source_write
```

**Expected:**
- A writable source (`zip_source_begin_write` on a buffer/file source) accepts
  `zip_source_write` and stores the bytes.
- `zip_source_seek_write` positions the write cursor (SEEK_SET/CUR/END) and
  subsequent writes land at the correct offset.
- After `zip_source_commit_write`, the written bytes are persisted and readable;
  the output matches C libzip for the same written content.
- `zip_source_write` on a non-writable source returns `-1` and sets a defined
  error (no panic).

---

## 5. TC-4b-3 — Write-mode lifecycle: begin → commit / rollback

**Command:**
```bash
cargo test --package zip-sys source_commit_rollback
```

**Expected:**
- `zip_source_begin_write(src)` succeeds; bytes written via `zip_source_write`
  accumulate.
- `zip_source_commit_write` persists the buffer — reading the source afterward
  returns exactly the written bytes.
- On a separate source, `zip_source_rollback_write` discards all written bytes —
  the source returns to its pre-write state (empty/unchanged).
- `zip_source_begin_write_cloning` clones an existing source's contents as the
  write base, and edits on the clone do not affect the original.
- Lifecycle misordering (write before begin, commit after rollback) returns a
  defined error, never a panic.

---

## 6. TC-4b-4 — Full pipeline `file → window → crc → compress`, byte-identical to C

**Command:** differential harness (extend `cross_read`/a new bin) + unit test:
```bash
cargo test --package zip-sys source_pipeline_file_window_crc_compress
bash run-verify.sh
```

**Expected:**
- A C consumer can build `file → window → crc → compress` over the same input
  bytes through the Rust cdylib and through C libzip; the resulting compressed
  output is **byte-identical** (diff the outputs).
- The `window` selects the correct byte range; `crc` computes the same CRC-32 as
  C; `compress` (Store/Deflate) produces byte-identical compressed bytes.
- `zip_source_make_command_bitmap`, `zip_source_pass_to_lower_layer`,
  `zip_source_seek_compute_offset`, and `zip_source_args_seek` helpers return
  values identical to C.
- Reading the pipeline output back (or feeding it as an archive entry) yields
  the original bytes.

---

## 7. TC-4b-5 — `zip_source_*` helper functions match C

**Command:**
```bash
cargo test --package zip-sys source_helpers_parity
```

**Expected:** for identical inputs, return values match C libzip:
- `zip_source_make_command_bitmap(...)` produces the same command bitmap.
- `zip_source_pass_to_lower_layer(...)` routes commands to the lower layer and
  returns the same result.
- `zip_source_seek_compute_offset(...)` computes the same target offset.
- `zip_source_args_seek(...)` handles the seek args struct the same way.

---

## 8. TC-4b-6 — No panic on malformed write/layered callbacks

**Command:**
```bash
cargo +nightly fuzz run fuzz_entry_reader
cargo +nightly fuzz run fuzz_central_dir
```
plus robustness unit tests:
```bash
cargo test --package zip-sys source_write_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: a write callback
returning invalid write counts, a layered source whose upper callback returns
invalid commands, begin_write on an already-open source, commit_write with no
pending write, and rollback after commit. Malformed inputs yield a defined error
code, never a panic.

---

## 9. New C ABI symbols required (engineer must export)

`zip_source_layered`, `zip_source_layered_create`, `zip_source_write`,
`zip_source_seek_write`, `zip_source_begin_write`,
`zip_source_begin_write_cloning`, `zip_source_commit_write`,
`zip_source_rollback_write`, `zip_source_make_command_bitmap`,
`zip_source_pass_to_lower_layer`, `zip_source_seek_compute_offset`,
`zip_source_args_seek`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all 12 new symbols resolve via `libloading`/`dlopen`.

---

## 10. Pass criteria (summary) — Phase 4b

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions, incl. 4a read-side tests).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-4b-1: layered source wraps a lower source correctly.
4. TC-4b-2: `zip_source_write`/`seek_write` write-mode behavior matches C.
5. TC-4b-3: begin→commit persists; begin→rollback discards; cloning isolates.
6. TC-4b-4: `file → window → crc → compress` pipeline output byte-identical to C.
7. TC-4b-5: source helpers return values identical to C.
8. TC-4b-6: no panic on malformed write/layered callbacks (fuzz + unit).
9. All 12 new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer-4b` (orchestration
loop step 5).

# Phase 5 — `zip_open_from_source` / `zip_fdopen` + write-mode sources — Test Cases

**Phase:** 5 · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 5.
**Depends on:** Phases 1–4 (encryption, metadata, streaming sources). Phase 4
(4a+4b) — DONE (`f6eb17c`, `b2afeb7`).
**Constraint:** `zip-core` core logic may be modified only where Phase 5 requires
it. Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Current-state reconciliation (READ FIRST)

- The following Phase-5 write-mode/helper symbols were **already implemented in
  Phase 4b** (`b2afeb7`) and are exported — **do not re-assign the implementation**;
  Phase 5 re-verifies their behavior as regression tests only:
  `zip_source_begin_write`, `zip_source_begin_write_cloning`,
  `zip_source_commit_write`, `zip_source_rollback_write`,
  `zip_source_make_command_bitmap`, `zip_source_pass_to_lower_layer`,
  `zip_source_seek_compute_offset`, `zip_source_args_seek`.
- The **genuinely new Phase-5 deliverables are `zip_open_from_source` and
  `zip_fdopen`** — both are currently MISSING from `zip-sys` (only referenced in a
  doc comment). `zip_open(path, flags, errp)` already exists (~1097) and is the
  model for `zip_open_from_source`.
- `zip-core` already has `Archive::open` over in-memory/`Cursor`/file data; Phase 5
  wires it to open from a `zip_source_t` (buffer/file) and from a file descriptor.

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass (incl. Phase 4a/4b source
tests). Any pre-existing failure here means STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 5):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The read
path must remain byte-identical.

---

## 2. TC-1 — `zip_open_from_source` opens an archive from a buffer/file source

**Command:**
```bash
cargo test --package zip-sys open_from_source_buffer
cargo test --package zip-sys open_from_source_file
```

**Expected:**
- `zip_source_buffer_create(data, len, 0, &err)` → `zip_open_from_source(src,
  flags, &err)` opens a valid archive; `zip_get_num_entries` and per-entry reads
  are **byte-identical** to opening the same archive by path with `zip_open`.
- `zip_source_file_create(path, ...)` → `zip_open_from_source` opens the archive
  identically.
- Ownership: the source is consumed/freed by the archive per C libzip semantics
  (no double-free, no leak).

---

## 3. TC-2 — `zip_open_from_source` error codes match C (NOZIP / TRUNCATED / INCONS)

**Command:**
```bash
cargo test --package zip-sys open_from_source_errors
```

**Expected:** for the same malformed inputs, `err` matches C libzip:
- Non-zip garbage buffer → `ZIP_ER_NOZIP` (`19`).
- Truncated archive → `ZIP_ER_TRUNCATED_ZIP` (`35`).
- Inconsistent archive (e.g. bad central-dir offsets) → `ZIP_ER_INCONS` (`21`).
- `zip_strerror` on the archive returns the matching C string for each.
- These match the codes already proven for the path-based open path in
  `verification-report.md` §2.3 (NOZIP `19`, TRUNCATED `35`, INCONS `21`).

---

## 4. TC-3 — `zip_fdopen` wraps a file descriptor; reads match C

**Command:**
```bash
cargo test --package zip-sys fdopen_read
```

**Expected:**
- `zip_fdopen(dup(fd), flags, &err)` wraps an open fd over a `.zip`; the archive
  opens and every entry reads **byte-identically** to the same archive opened by
  path (FNV/SHA-256 compare).
- `zip_fdopen` takes ownership of the fd per C semantics (archive close closes
  the fd; a failed open still closes it).
- Passing an invalid/unreadable fd yields the matching C error (e.g. `ZIP_ER_OPEN`
  `11`) and does not panic.

---

## 5. TC-4 — Write-mode source lifecycle behaves like C

**Command:** regression re-verification of the Phase-4b implementation:
```bash
cargo test --package zip-sys source_commit_rollback
cargo test --package zip-sys source_begin_write_cloning
```

**Expected:**
- `zip_source_begin_write(src)` → `zip_source_write(...)` accumulates bytes →
  `zip_source_commit_write` persists; reading the source returns the written
  bytes.
- `zip_source_rollback_write` **discards** all pending writes (source returns to
  its pre-write state); nothing partial is committed.
- `zip_source_begin_write_cloning` clones the original as the write base; edits
  on the clone do not affect the original.
- Lifecycle misordering returns a defined error, never a panic.

---

## 6. TC-5 — Utility helpers return values identical to C

**Command:** regression re-verification of the Phase-4b implementation:
```bash
cargo test --package zip-sys source_helpers_parity
```

**Expected:** for identical inputs, return values match C libzip:
- `zip_source_make_command_bitmap(...)` — same command bitmap.
- `zip_source_pass_to_lower_layer(...)` — same routed result.
- `zip_source_seek_compute_offset(...)` — same target offset.
- `zip_source_args_seek(...)` — same seek result from the args struct.

---

## 7. TC-6 — No panic on malformed source / fdopen input

**Command:**
```bash
cargo +nightly fuzz run fuzz_entry_reader
cargo +nightly fuzz run fuzz_central_dir
```
plus robustness unit tests:
```bash
cargo test --package zip-sys open_from_source_malformed_no_panic
cargo test --package zip-sys fdopen_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: opening from a
NULL/invalid source pointer, opening from a buffer whose length is 0, passing an
invalid fd, and a source whose read callback returns garbage/negative counts.
Malformed inputs yield a defined error code (`ZIP_ER_NOZIP`, `ZIP_ER_INCONS`,
`ZIP_ER_READ`, `ZIP_ER_OPEN`), never a panic.

---

## 8. New C ABI symbols required (engineer must export)

- **New for Phase 5:** `zip_open_from_source`, `zip_fdopen`.
- **Already exported (Phase 4b) — verify resolve, do not re-add:**
  `zip_source_begin_write`, `zip_source_begin_write_cloning`,
  `zip_source_commit_write`, `zip_source_rollback_write`,
  `zip_source_make_command_bitmap`, `zip_source_pass_to_lower_layer`,
  `zip_source_seek_compute_offset`, `zip_source_args_seek`.

Verify:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** `zip_open_from_source` and `zip_fdopen` resolve via
`libloading`/`dlopen`; the 8 Phase-4b symbols still resolve.

---

## 9. Pass criteria (summary)

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions, incl. Phase 4a/4b tests).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-1: `zip_open_from_source` opens from buffer and file sources, reads
   byte-identical to `zip_open`.
4. TC-2: `zip_open_from_source` error codes match C (NOZIP 19 / TRUNCATED 35 /
   INCONS 21) + matching `zip_strerror` strings.
5. TC-3: `zip_fdopen` wraps an fd; reads match C; ownership + error paths correct.
6. TC-4: write-mode lifecycle (begin/commit/rollback/clone) behaves like C.
7. TC-5: helper functions return values identical to C.
8. TC-6: no panic on malformed source/fdopen input (fuzz + unit).
9. `zip_open_from_source` + `zip_fdopen` resolve; Phase-4b helper symbols still
   resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

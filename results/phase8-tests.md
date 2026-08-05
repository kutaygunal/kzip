# Phase 8 — Win32 sources + source utility helpers — Test Cases

**Phase:** 8 · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 8.
**Depends on:** Phases 1–7. Phase 7 — DONE (`6b032a4`).
**Constraint:** `zip-core` core logic may be modified only where Phase 8 requires
it. Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Platform note & current-state reconciliation (READ FIRST)

- This is a **Windows** host (`C:/Users/...`, MSVC build) and C libzip is
  `libs/c/zip.dll`. Win32 sources (`zip_source_win32a/w/...`) use Windows
  `HANDLE`s and are inherently Windows-specific. If any of these are ever built
  on non-Windows, the harness must be `#[cfg(windows)]`-gated; on this host they
  are live.
- `zip-core` has **no Win32 source or `zip_buffer_fragment` implementation yet**:
  `source.rs` implements `Source` for `File` and `Cursor`, not for Win32
  `HANDLE`s; there is no `zip_buffer_fragment`.
- `zip-sys` exports **none** of the Phase 8 symbols (only a doc-comment mention).
- All eight symbols below are **new deliverables**.

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass. Any pre-existing failure here
means STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 8):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The read
path must remain byte-identical.

---

## 2. TC-1 — Win32 ANSI/wide/handle sources open + read identically to the stdio file source

**Command:**
```bash
cargo test --package zip-sys win32_source_parity
```
(gated `#[cfg(windows)]`)

**Expected:** for the same `.zip` file, all three source flavors produce
**byte-identical** reads:
- `zip_source_win32a_create(path_ansi, 0, -1, &err)` — ANSI `char*` path.
- `zip_source_win32w_create(path_wide, 0, -1, &err)` — UTF-16 `wchar_t*` path.
- `zip_source_win32handle_create(hFile, 0, -1, &err)` — an open Windows file
  `HANDLE`.
- Each opens the archive via `zip_open_from_source` and every entry reads
  byte-identically to the same archive opened by the stdio file source
  (`zip_source_file`), by FNV/SHA-256 compare.
- The wide variant handles non-ASCII path content (e.g. Unicode/emoji filenames).
- The `zip_source_win32a` / `zip_source_win32w` / `zip_source_win32handle`
  (non-`_create`) open variants behave identically to their `_create` counterparts.

---

## 3. TC-2 — `zip_buffer_fragment` accepts a fragment array and reads correctly

**Command:**
```bash
cargo test --package zip-sys buffer_fragment
```

**Expected:**
- `zip_buffer_fragment` accepts an array of `zip_buffer_fragment_t`
  `{offset, length, data}` fragments describing a discontiguous buffer.
- Reading an archive assembled from the fragments yields the same bytes as the
  contiguous equivalent (i.e. fragments in order equal the single buffer).
- Fragment boundaries are honored; a fragment with a bad offset/length yields a
  defined error (not a panic).
- Return values / error handling match C libzip for identical fragment inputs.

---

## 4. TC-3 — All helpers return values identical to C

**Command:**
```bash
cargo test --package zip-sys win32_helpers_parity
```

**Expected:** for identical inputs, the new symbols return values identical to C
libzip:
- `zip_source_win32a` / `zip_source_win32w` / `zip_source_win32handle` and their
  `_create` forms return the correct source handle (or `NULL` + error) for valid /
  invalid inputs.
- `zip_buffer_fragment` reads the same number of bytes and returns the same
  status as C for identical fragment arrays.
- Stat/seek/at_eof behavior through each Win32 source matches the stdio file
  source.

---

## 5. TC-4 — No panic on invalid handles

**Command:**
```bash
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_entry_reader
```
plus robustness unit tests:
```bash
cargo test --package zip-sys win32_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: an invalid/closed
`HANDLE`, a NULL ANSI/wide path, a path to a non-existent file, a
`zip_buffer_fragment` array with NULL data or inconsistent length, and a fragment
whose offsets exceed the buffer. Invalid inputs yield a defined error (e.g.
`ZIP_ER_OPEN`, `ZIP_ER_INVAL`, `ZIP_ER_READ`), never a panic.

---

## 6. New C ABI symbols required (engineer must export)

`zip_source_win32a`, `zip_source_win32a_create`, `zip_source_win32w`,
`zip_source_win32w_create`, `zip_source_win32handle`,
`zip_source_win32handle_create`, `zip_buffer_fragment`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all seven symbols resolve via `libloading`/`dlopen` on Windows.

---

## 7. Pass criteria (summary) — FINAL PHASE

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-1: Win32 ANSI/wide/handle sources open + read archives byte-identically to
   the stdio file source.
4. TC-2: `zip_buffer_fragment` accepts a fragment array and reads correctly.
5. TC-3: all new helpers return values identical to C.
6. TC-4: no panic on invalid handles / malformed input (fuzz + unit).
7. All seven new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

---

## 8. Post-Phase 8 — closure note

Phase 8 is the **final** phase in the plan. When it passes, the orchestrator
should:
- Re-run the **full** verification suite once more (`bash run-verify.sh`) as a
  final whole-project gate.
- Have `devops` commit `results/phase8-tests.md` and the Phase 8 implementation.
- Confirm the `ORCHESTRATION.md` tracker marks all 8 phases `DONE`.
- Declare the gap-closure project complete (all §5 prioritized items addressed:
  Critical/High items already closed pre-plan; Phases 1–8 closed the remaining
  Medium/Low–Med items).

# Phase 6 — Progress & cancel callbacks — Test Cases

**Phase:** 6 · **Priority:** Low–Med · **§5 item:** 10
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 6.
**Depends on:** Phases 1–5 (encryption, metadata, streaming sources, open-from-
source). Phase 5 — DONE (`88a6ebc`).
**Constraint:** `zip-core` core logic may be modified only where Phase 6 requires
it (cooperative poll points in the write/compress path). Re-run `run-verify.sh`
after the phase (plan §3.5) to prove no read-path regression.

---

## 0. Current-state reconciliation (READ FIRST)

- `zip-core` has **no progress/cancel infrastructure yet**: `compress.rs`
  (`CompressOptions`) has no progress/cancel/callback fields, and there are no
  cooperative poll points. This is the Phase 6 deliverable (PLAN.md §5.1
  "cooperative poll points").
- `zip-sys` exports **no** `zip_register_progress_callback*` or
  `zip_register_cancel_callback_with_state` symbols.
- Reference error codes already present: `ZIP_ER_OPNOTSUPP` = `28`,
  `ZIP_ER_CANCELLED` = `32`.
- Reference C ABI (libzip): `zip_register_progress_callback(zh, fn)` →
  `zip_register_progress_callback_with_state(zh, fn, ud)` →
  `zip_register_cancel_callback_with_state(zh, fn, ud)`. Progress is reported via
  `zip_progress_t` (fields `precision`, `bytes_completed`, `bytes_total`).

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass. Any pre-existing failure here
means STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 6):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS` and `[cross_read] ... all_match=true`. The read
path must remain byte-identical.

**Determinism gate (plan §2 Phase 6):** byte-identical output must be UNCHANGED
when callbacks are registered vs not registered (see TC-4).

---

## 2. TC-1 — Progress callback fires monotonically; final reaches 1.0

**Command:**
```bash
cargo test --package zip-sys progress_callback_monotonic
```
(or an in-process `zip-core` test `cargo test --package zip-core progress_monotonic`)

**Expected:**
- Registering `zip_register_progress_callback_with_state` on an archive that is
  being written/compressed causes the callback to fire one or more times during
  the operation.
- The reported `zip_progress_t` values are **monotonically non-decreasing**: each
  sample satisfies `p.bytes_completed <= next.bytes_completed` and
  `p.bytes_completed <= p.bytes_total`.
- The **final** callback reports `bytes_completed == bytes_total` (progress reaches
  `1.0` / fully complete).
- The callback receives the correct user state pointer (see TC-3).

---

## 3. TC-2 — Cancel callback (non-zero) aborts → `ZIP_ER_CANCELLED`(32) / `ZIP_ER_OPNOTSUPP`(28)

**Command:**
```bash
cargo test --package zip-sys cancel_callback_aborts
```
(or in-process `cargo test --package zip-core cancel_aborts`)

**Expected:**
- Registering `zip_register_cancel_callback_with_state` and having the callback
  return **non-zero** during compression aborts the operation.
- The operation returns one of `ZIP_ER_OPNOTSUPP` (`28`) or `ZIP_ER_CANCELLED`
  (`32`) — matching C libzip semantics for the abort path.
- **No partial/corrupt output is committed**: the destination archive/file is
  left in a valid state (either unchanged or with the aborted write rolled back),
  never a truncated or corrupted archive on disk.
- The callback returning `0` does NOT abort; the operation completes normally.

---

## 4. TC-3 — Callbacks with user state receive the correct state pointer

**Command:**
```bash
cargo test --package zip-sys callback_state_pointer
```

**Expected:**
- For both `zip_register_progress_callback_with_state` and
  `zip_register_cancel_callback_with_state`, the `ud`/state pointer passed at
  registration is received **unchanged** by every callback invocation.
- Mutating that state from within the callback (e.g. incrementing a counter or
  setting a flag) is observed by the caller after the operation.
- The plain (non-`_with_state`) `zip_register_progress_callback` variant still
  works with no state pointer.

---

## 5. TC-4 — Deterministic output unchanged when callbacks are registered

**Command:**
```bash
cargo test --package zip-sys output_deterministic_with_callbacks
bash run-verify.sh
```

**Expected:**
- Writing the same archive (same inputs, same compression settings) with and
  without callbacks registered produces **byte-identical** output (diff the two
  archives).
- Registering callbacks must not change the produced bytes — only observe/cancel.
- The read path remains byte-identical per the `run-verify.sh` gate.

---

## 6. TC-5 — No panic on malformed callback input

**Command:**
```bash
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_codec
```
plus robustness unit tests:
```bash
cargo test --package zip-core callbacks_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: registering a
NULL/`None` callback, a callback that is invoked after the archive is closed, a
cancel callback that returns a garbage non-zero value on every poll, and a
progress callback over a zero-length archive. Malformed inputs yield a defined
error, never a panic.

---

## 7. New C ABI symbols required (engineer must export)

`zip_register_progress_callback`, `zip_register_progress_callback_with_state`,
`zip_register_cancel_callback_with_state`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all three symbols resolve via `libloading`/`dlopen`.

---

## 8. Pass criteria (summary)

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-1: progress callback fires with monotonically non-decreasing
   `zip_progress_t`; final `bytes_completed == bytes_total`.
4. TC-2: non-zero cancel callback aborts → `ZIP_ER_CANCELLED`(32) /
   `ZIP_ER_OPNOTSUPP`(28); no partial/corrupt output committed.
5. TC-3: `_with_state` callbacks receive the correct state pointer; plain variant
   works.
6. TC-4: output byte-identical with vs without callbacks registered.
7. TC-5: no panic on malformed callback input (fuzz + unit).
8. All three new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

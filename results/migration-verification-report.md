# kzip — COMPLETE-MIGRATION Verification Report

**Date:** 2024-08-04
**Orchestrator:** complete-migration verification orchestrator
**Method:** the migration was partitioned into 8 independent verification
chunks. Each chunk was verified by a dedicated subagent (a separate `pi`
instance in its own herdr pane). Each subagent was a **VERIFIER ONLY** — it
reported gaps but was explicitly forbidden from modifying core logic. After
each subagent returned its report, the orchestrator **stopped** it (closed its
pane) before spawning the next wave. All 8 chunk subagents were spawned and
stopped.

**Binaries under test:**
- **C libzip:** `libs/c/zip.dll` (v1.11.4, MSVC release). Public API in
  `libzip/lib/zip.h` (~128–131 `zip_*` symbols).
- **Rust cdylib:** `target/release/zip.dll` (crate `zip-sys`, READ-ONLY ABI subset).
- **Rust in-process core:** `crates/zip-core`; async: `crates/zip-async`; tools: `crates/ziptools`.

**Baseline:** prior `results/verification-report.md` documented several
correctness gaps that were subsequently fixed in commit
`29ddb0c "Fix correctness gaps to match C libzip (ZIP64, stat, mtime, errors, ABI)"`.
This audit re-verifies the current `main` state and re-classifies every gap.

---

## 1. Per-Chunk Results

| # | Chunk | Subagent | Verdict | Evidence | Gaps found |
|---|---|---|---|---|---|
| 1 | read-verify | `read-verify` | **COMPLETE** | Read-path differential harness (`differential/src/bin/verify_read.rs`) run against both C dll and Rust cdylib; `results/verify-read.json` vs `verify-read-rust.json` diff **empty** (byte-identical). Decompressed content (FNV-1a), `zip_get_name`, `num_entries`, `zip_fopen`/`zip_fopen_index`, and all stat size fields identical for every openable entry across all 17 archives. The 70 000-entry `many_zip64.zip` now **opens and reads correctly in both** (ZIP64 fix confirmed). | None (read-path). |
| 2 | write-verify | `write-verify` | **COMPLETE** | Cross-read harness (`differential/src/bin/cross_read.rs`): **(a) C writes → Rust reads** — every C-generated archive read by `zip_core` byte-identical to ground truth (71 517/71 517 entries); **(b) Rust writes → C reads** — Rust `write_archive` (deflate 6) output read back byte-identically by C libzip (5/5 entries, names/sizes match). | None (write-path). |
| 3 | meta-verify | `meta-verify` | **PARTIAL** | `zip_stat_t` layout parity **PASS** (incl. trailing `flags` field); `valid` bitmask == C `ZIP_STAT_*` bits (not `0xFFFFFFFF`); unencrypted `encryption_method` == `ZIP_EM_NONE`(0); `mtime` parity; `size/comp_size/crc/comp_method/name/index` parity; numeric error-code parity (NOZIP 19, TRUNCATED_ZIP 35, EF_TOO_LARGE 36, INCONS 21, NOENT 9, INVAL 18, OPEN 11) all **PASS**. | **Error-string wording/capitalization parity** in `crates/zip-sys/src/lib.rs` `err_str`: NOZIP, TRUNCATED_ZIP, EF_TOO_LARGE, OPEN, INCONS (missing arm → generic `"zip error"`) and several lowercase vs C sentence-case. Only NOENT/INVAL match. Coverage gap: no corpus archive exercises EF_TOO_LARGE(36) string at runtime. |
| 4 | abi-verify | `abi-verify` | **PARTIAL** | Enumerated all 128 public C functions in `libzip/lib/zip.h` vs the `#[no_mangle]` exports in `crates/zip-sys/src/lib.rs`. **14 IMPLEMENTED** (all ABI-signature-correct): `zip_open`, `zip_close`, `zip_get_num_entries`, `zip_get_name`, `zip_strerror`, `zip_file_strerror`, `zip_fopen`, `zip_fopen_index`, `zip_fread`, `zip_fclose`, `zip_stat_init`, `zip_stat`, `zip_stat_index`, `zip_libzip_version`. | **114 of 128 C functions not exported (89.1%)** — 1 NOT-EXPORTED (`zip_name_locate`, present in `zip-core` but not in the cdylib) + 113 MISSING. Missing capabilities: write/edit path, encryption, `zip_source_*` streaming (38), `zip_error_*` object API (12), comments/extra-fields/attributes, fseek/ftell, fdopen, progress/cancel callbacks, Win32 sources, misc. |
| 5 | parallel-verify | `parallel-verify` | **COMPLETE** | Scratch determinism test: parallel output **byte-identical** to serial. Scaling measured: 2.97× on 16 files, ~5.7× on 64 files (8 workers), 7.14× in committed mixed benchmark (24 workers). No nondeterminism in entry ordering/timestamps/thread scheduling. | Design limitation (not a bug): parallelism is per-file (single large file gets no win); all-or-nothing large-file fallback (a corpus with one ≥8 MiB file runs fully serial). |
| 6 | async-verify | `async-verify` | **COMPLETE** | `cargo test -p zip-async --all-features` clean; async read output **byte-identical** to sync `zip-core` read across corpus (binary/nested/text + handcrafted error cases); error behavior matches sync exactly. `many_zip64.zip` (70k) and `huge.zip` (8 MiB) exercised the streaming path. | No `tests/` dir (coverage in `src/lib.rs` unit tests). Async is a documented bridge mode (`spawn_blocking` around the sync engine), not a native poll path — documented design choice, correctness preserved. |
| 7 | security-verify | `security-verify` | **PARTIAL** | `unsafe` **fully confined to the FFI boundary** (PASS). **No reachable panic on external/malformed input** (PASS): 9 robustness tests + handcrafted-corpus smoke (0 panics; bad→Nozip, truncated→TruncatedZip, zip64→Incons). Allocation caps present: `MAX_CD_BUFFER`, `ZERO_COPY_MAX_UNCOMP`=32 MiB, bounded `BufferPool`. | **2 confirmed zip-bomb gaps (correctness/security):** (A) `crates/zip-core/src/codec.rs` `decode_slice_into` (zero-copy path) has **no cap on actual decompressed output** — a 65 KB deflate stream decompressed to 64 MiB; (B) `crates/zip-core/src/cdir.rs` `read_entries` does `vec![0u8; cdir_size]` with **no cap** — an EOCD claiming `u32::MAX` → ~4 GiB allocation → OOM/DoS. Minor: `zip-async` `expect("lock")` poisoned-mutex panic sites; `zip-sys:zip_open` unbounded whole-file `read_to_end`. Fuzz smoke **NOT RUN** (cargo-fuzz not installed; no-fuzz robustness tests pass). |
| 8 | bench-verify | `bench-verify` | **COMPLETE** | Benchmark suite builds and runs cleanly; reports produced (`results/benchmark-*.csv`); C-vs-Rust parity claim supported with **no regression** vs prior results. | Pre-existing (not a regression): Rust/C serial ratio 85.4% (prior 84.4%) below the 90% acceptance gate — documented codec-backend finding (miniz_oxide vs native zlib). Criterion noise flags on `compress_serial`/zerocopy benches are noise vs saved baseline. Report `.md/.html` are static (bins only write CSVs). |

**Chunk spawn/stop accounting:** 8 distinct chunks verified. 9 subagent
spawns (abi-verify was respawned once after its first pane was lost to an
interrupt before it returned). All 8 chunk subagents were **stopped** (pane
closed) after returning; verified via `herdr agent list` that no chunk agent
remained before spawning the next wave.

---

## 2. Migration Completeness Verdict

**The Rust port is a COMPLETE migration of the C libzip READ path, and a
byte-identical, deterministic, memory-safe-on-valid-input implementation of
the core engine — but it is NOT yet a complete migration of the full C libzip
public API surface.**

- **Read path: COMPLETE.** Byte-identical decompressed content, names, sizes,
  stat fields, and numeric error codes vs C libzip across an edge-case corpus,
  including the previously-broken 70 000-entry ZIP64 archive.
- **Write path: COMPLETE at the format level.** Rust-written archives are read
  byte-identically by C and vice-versa (verified both directions), but the
  write path is **not exported through the `zip-sys` C ABI**.
- **Parallel & async: COMPLETE.** Deterministic (byte-identical to serial) and
  correct (async == sync).
- **Security: PARTIAL.** Memory-safe on valid and malformed input (no reachable
  panics, unsafe confined to FFI), but two **zip-bomb / unbounded-allocation
  gaps** remain in the core reader.
- **ABI surface: PARTIAL.** Only 14 of 128 C functions are exported by the
  cdylib (89.1% of the C API is not exposed).

---

## 3. Remaining Gaps

### 3.1 Correctness / security bugs (should be fixed)

| # | Severity | Gap | Location |
|---|---|---|---|
| 1 | **High (security)** | Zip-bomb: zero-copy `decode_slice_into` has no cap on actual decompressed output (claimed size ≤32 MiB but real output unbounded). | `crates/zip-core/src/codec.rs` |
| 2 | **High (security)** | Unbounded central-directory allocation: `vec![0u8; cdir_size]` with no cap; EOCD/ZIP64 claim of `u32::MAX` → ~4 GiB allocation → OOM/DoS. | `crates/zip-core/src/cdir.rs` `read_entries` |
| 3 | **Low** | Error-string wording/capitalization parity with libzip (`err_str`): NOZIP, TRUNCATED_ZIP, EF_TOO_LARGE, OPEN, INCONS (missing arm → generic `"zip error"`), lowercase vs sentence-case. | `crates/zip-sys/src/lib.rs` |
| 4 | **Low** | `zip-async` `expect("lock")` on poisoned-mutex panic sites (not reachable from external input). | `crates/zip-async/src/lib.rs` |
| 5 | **Low** | `zip-sys:zip_open` unbounded whole-file `read_to_end` (FFI-layer design choice). | `crates/zip-sys/src/lib.rs` |

### 3.2 Missing capabilities (not bugs — absent functionality)

| # | Capability | Status | Notes |
|---|---|---|---|
| 1 | **Write/edit path via C ABI** | MISSING | `zip-core` has a batch `write_archive` engine, but no C-ABI write/edit functions (`zip_file_add`, `zip_dir_add`, `zip_delete`, `zip_rename`, `zip_replace`, `zip_discard`, `zip_close` write semantics). |
| 2 | **Encryption** (ZipCrypto + WinZip AES, read & write) | MISSING | `zip_fopen_encrypted`, `zip_fopen_index_encrypted`, `zip_set_default_password`, `zip_file_set_encryption`. |
| 3 | **`zip_source_*` streaming sources** (38 symbols) | MISSING | buffer/file/function/layered/window/zip sources; user-defined source ABI. |
| 4 | **`zip_error_*` structured error object API** (12 symbols) | MISSING | `zip_get_error`, `zip_error_init/clear/set/...`. |
| 5 | **Comments / extra-fields / attributes** (write + full read) | MISSING | archive & file comments (write), extra-field API, external attributes, dostime. |
| 6 | **fseek / ftell / seekability** | MISSING | `zip_fseek`, `zip_ftell`, `zip_file_is_seekable`. |
| 7 | **Progress / cancel callbacks** | MISSING | `zip_register_progress_callback*`, `zip_register_cancel_callback_with_state`. |
| 8 | **Misc** | MISSING | `zip_fdopen`, `zip_open_from_source`, `zip_get_archive_flag`, `zip_compression_method_supported`, `zip_encryption_method_supported`, Win32 sources, `zip_unchange*`. |
| 9 | **`zip_name_locate`** | NOT-EXPORTED | present in `zip-core`, missing from the cdylib ABI. |

### 3.3 Design limitations / coverage (not defects)

- Parallel compression is per-file; a single large file gets no parallel win,
  and a corpus containing one ≥8 MiB file falls back to fully serial.
- Async is a bridge mode (`spawn_blocking`), not a native poll path (documented).
- Fuzz smoke not run (cargo-fuzz not installed); no-fuzz robustness tests pass.
- Benchmark serial gate (85.4% vs 90%) unmet — pre-existing codec-backend finding.
- No corpus archive exercises the EF_TOO_LARGE(36) error string at runtime.

---

## 4. Prioritized Recommendation

1. **Fix the two zip-bomb gaps first** (High, security): cap decompressed output
   in `codec.rs::decode_slice_into` and cap `cdir_size` allocation in
   `cdir.rs::read_entries`. These are the only remaining memory-safety /
   DoS issues in the core reader and are small, localized fixes.
2. **Close the error-string parity gap** (Low): align `zip-sys::err_str` with
   libzip's exact strings (add the missing INCONS arm, sentence-case).
3. **Expand the `zip-sys` ABI** in priority order to reach drop-in C
   compatibility: write/edit path → `zip_name_locate` (already in core) →
   `zip_error_*` → comments/extra-fields → fseek/ftell → progress/cancel.
4. **Encryption and `zip_source_*` streaming** are the two largest remaining
   capability gaps; schedule as a dedicated phase.
5. **Add a fuzz target** (or CI fuzz smoke) to continuously exercise the reader
   against malformed input now that the robustness tests pass.

---

## 5. Artifacts

- `results/migration-verification-report.md` — this report.
- `results/verify-read.json` / `verify-read-rust.json` / `verify-read.diff` — read-path diff (empty = byte-identical).
- `results/verify-crossread.json` — cross-read / write-path result.
- `results/benchmark-*.csv`, `c-serial.csv`, `rust-serial.csv` — regenerated benchmark data.
- `data/corpus-verify/` — edge-case corpus + ground truth.
- Harness: `differential/src/bin/{gen_corpus,verify_read,cross_read}.rs`, `run-verify.sh`.

### Constraints honored
- No chunk subagent modified core crate logic; all were verifier-only.
- Each chunk subagent was stopped after returning its report.
- `git status` shows only regenerated benchmark CSVs (suite output) plus this report — no core source changes.

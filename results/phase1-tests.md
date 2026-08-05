# Phase 1 — Encryption: ZipCrypto (PKWARE) read + write — Test Cases

**Phase:** 1 · **Priority:** Medium · **§5 item:** 7
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 1.
**Constraint:** `zip-core` core logic may be modified only where Phase 1 requires
it (encryption). Read-path equivalence already proven must NOT regress — re-run
`run-verify.sh` after the phase (plan §3.5).

---

## 0. Reference values (ground truth)

| Constant | Value | C string (`zip_strerror`) |
|---|---|---|
| `ZIP_ER_NOPASS` | `26` | `"No password provided"` |
| `ZIP_ER_WRONGPASS` | `27` | `"Wrong password provided"` |
| `ZIP_ER_ENCRNOTSUPP` | `24` | `"Encryption method not supported"` |
| `ZIP_EM_TRAD_PKWARE` | `1` | — (stat field, not a string) |
| `ZIP_EM_NONE` | `0` | — |

These strings are already byte-matched to libzip in `zip-sys::err_str` (see
`results/verification-report.md` §4.2 / `36dcc81`); Phase 1 must keep them
exact for the new error paths.

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass. If any pre-existing test
fails here, STOP and report to the engineer — do not attribute it to Phase 1.

**Regression gate (must stay PASS after Phase 1):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS (byte-identical JSON across all archives)` and
`[cross_read] ... all_match=true` in `results/verify-crossread.err`. This proves
the unencrypted read path did not regress.

---

## 2. TC-1 — Byte-identical read of C-written ZipCrypto archives

**Setup:** `gen_corpus` must be extended to also emit ZipCrypto-encrypted
archives (e.g. `enc_zipcrypto.zip`, several entries, deflate + store, a known
password such as `"kzip-test-password"`), with ground-truth inputs mirrored under
`data/corpus-verify/inputs/enc_zipcrypto/<index>`.

**Command:**
```bash
bash run-verify.sh
# then, for the encrypted corpus specifically:
target/release/verify_read.exe libs/c/zip.dll data/corpus-verify > results/verify-read.json
target/release/verify_read.exe target/release/zip.dll data/corpus-verify > results/verify-read-rust.json
diff -u results/verify-read.json results/verify-read-rust.json
```

**Expected:**
- The encrypted archive opens in BOTH libraries (no `ZIP_ER_ENCRNOTSUPP`).
- Every entry's `fopen`/`fopen_index` FNV-1a fingerprint and length are
  **byte-identical** between C and Rust.
- `stat.encryption_method == 1` (`ZIP_EM_TRAD_PKWARE`) in both.
- `diff` produces **no output** (empty `results/verify-read.diff`).

**Note:** the harness must supply the password (via `zip_set_default_password`
or `zip_fopen_encrypted`) for the encrypted archive; otherwise it will hit the
NOPASS path (TC-3). The harness change is part of the engineer's Phase 1 work.

---

## 3. TC-2 — Byte-identical cross-read of Rust-written ZipCrypto archives

**Setup:** `cross_read` part (b) must be extended so `zip_core::write_archive`
(or the new encrypt-on-write path) writes a ZipCrypto-encrypted archive
(`data/corpus-verify/rust-written/rust_zipcrypto.zip`) with the same known
password, and the C library reads it back.

**Command:**
```bash
target/release/cross_read.exe libs/c/zip.dll data/corpus-verify
```

**Expected:** `[cross_read] C-writes/Rust-reads all_match=true,
Rust-writes/C-reads all_match=true` in `results/verify-crossread.err`. Every
Rust-written encrypted entry read by C libzip matches the ground-truth bytes
(SHA-256 + byte compare), names, and sizes.

---

## 4. TC-3 — Wrong password → `ZIP_ER_WRONGPASS`(27)

**Command:** a dedicated unit test in `zip-core` (e.g.
`crates/zip-core/src/crypto.rs` or `archive.rs`):
```bash
cargo test --package zip-core wrong_password
```
plus a C-ABI check through `zip-sys`:
```bash
cargo test --package zip-sys wrong_password
```

**Expected:**
- Opening an encrypted entry with an incorrect password fails.
- Error code is `ZIP_ER_WRONGPASS` = `27`.
- `zip_strerror` returns exactly `"Wrong password provided"` (byte-identical to C).

---

## 5. TC-4 — No password → `ZIP_ER_NOPASS`(26)

**Command:**
```bash
cargo test --package zip-core no_password
cargo test --package zip-sys no_password
```

**Expected:**
- Opening an encrypted entry with no password set fails.
- Error code is `ZIP_ER_NOPASS` = `26`.
- `zip_strerror` returns exactly `"No password provided"`.

---

## 6. TC-5 — `zip_strerror` strings match C

**Command:**
```bash
cargo test --package zip-sys err_str_matches_libzip_exactly
```
(extend the existing `err_str_matches_libzip_exactly` test to assert the
NOPASS/WRONGPASS/ENCRNOTSUPP entries are present and exact.)

**Expected:** the test passes; strings for codes `24`, `26`, `27` are
`"Encryption method not supported"`, `"No password provided"`,
`"Wrong password provided"` respectively.

---

## 7. TC-6 — `zip_file_set_encryption` + `zip_set_default_password` round-trip

**Command:** a C-ABI round-trip test through `zip-sys`:
```bash
cargo test --package zip-sys encryption_round_trip
```

**Expected:**
- `zip_set_default_password(zh, "kzip-test-password")` returns `0` and stores
  the password.
- `zip_file_set_encryption(zh, index, ZIP_EM_TRAD_PKWARE)` returns `0`.
- After `zip_close`/reopen, `zip_stat_index` reports
  `encryption_method == ZIP_EM_TRAD_PKWARE` (`1`).
- Reading the entry back with the correct password yields the original bytes
  (round-trip integrity).
- Setting an unsupported method returns an error (`ZIP_ER_ENCRNOTSUPP` = `24`).

---

## 8. TC-7 — `zip_stat` reports `ZIP_EM_TRAD_PKWARE`

**Command:**
```bash
cargo test --package zip-core stat_encryption_method_trad_pkw
cargo test --package zip-sys stat_encryption_method_trad_pkw
```

**Expected:** for an encrypted entry, `zip_stat`/`zip_stat_index` reports
`encryption_method == 1` (`ZIP_EM_TRAD_PKWARE`). For an unencrypted entry it
must still report `0` (`ZIP_EM_NONE`) — no regression of the §5 item-3 fix.

---

## 9. TC-8 — No panic on malformed/truncated encrypted input

**Command:** extend the existing fuzz targets to feed malformed/truncated
encrypted archives and ciphertext:
```bash
cargo +nightly fuzz run fuzz_entry_reader
cargo +nightly fuzz run fuzz_codec
cargo +nightly fuzz run fuzz_central_dir
```
plus a robustness unit test:
```bash
cargo test --package zip-core malformed_encrypted_no_panic
```

**Expected:**
- No panic, no abort, no unwrap on `None`/`Err` for: truncated encrypted local
  header, truncated ciphertext, wrong-size keystream, garbage after the 12-byte
  encryption header, and a valid header followed by truncated data.
- Malformed input yields a `Result::Err` with a defined error code (e.g.
  `ZIP_ER_CRC`, `ZIP_ER_TRUNCATED_ZIP`, or `ZIP_ER_WRONGPASS`), never a panic.

---

## 10. New C ABI symbols required (engineer must export)

`zip_fopen_encrypted`, `zip_fopen_index_encrypted`, `zip_file_set_encryption`,
`zip_set_default_password`. Verify they resolve:
```bash
target/release/verify_read.exe target/release/zip.dll data/corpus-verify   # loads lib
# or a symbol probe:
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all four symbols resolve via `libloading`/`dlopen`; the ABI test
passes.

---

## 11. Pass criteria (summary)

All of the following must hold for the phase to be handed to `devops`:

1. `cargo test --workspace` green (no regressions).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true (unencrypted
   path unchanged).
3. TC-1: C-written ZipCrypto reads byte-identically in Rust (empty diff).
4. TC-2: Rust-written ZipCrypto reads byte-identically in C.
5. TC-3/TC-4: WRONGPASS(27) and NOPASS(26) with exact C strings.
6. TC-5: `zip_strerror` strings byte-match C.
7. TC-6: set_encryption + set_default_password round-trip works.
8. TC-7: `zip_stat` reports `ZIP_EM_TRAD_PKWARE`(1); unencrypted still `0`.
9. TC-8: no panic on malformed encrypted input (fuzz + unit).
10. All four new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

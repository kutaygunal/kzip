# Phase 2 — Encryption: WinZip AES read + write — Test Cases

**Phase:** 2 · **Priority:** Medium · **§5 item:** 7
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 2.
**Depends on:** Phase 1 (ZipCrypto) — DONE, committed `27cdd73`; `crypto.rs`
exists and holds the ZipCrypto codec.
**Constraint:** `zip-core` core logic may be modified only where Phase 2 requires
it. Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Reference values (ground truth)

| Constant | Value | C string (`zip_strerror`) |
|---|---|---|
| `ZIP_ER_WRONGPASS` | `27` | `"Wrong password provided"` |
| `ZIP_ER_NOPASS` | `26` | `"No password provided"` |
| `ZIP_ER_CRC` | `7` | `"CRC error"` |
| `ZIP_EM_AES_128` | `0x0101` (`257`) | — (stat field) |
| `ZIP_EM_AES_192` | `0x0102` (`258`) | — (stat field) |
| `ZIP_EM_AES_256` | `0x0103` (`259`) | — (stat field) |

**Note:** `crates/zip-core/src/constant.rs::encryption` currently defines only
`NONE=0`, `TRAD_PKWARE=1`, `UNKNOWN=0xFFFF`. The three `ZIP_EM_AES_*` constants
must be **added** (they are part of the Phase 2 deliverable) so `dirent.rs` can
surface them in `zip_stat`.

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass (including Phase 1's
`encryption_round_trip` / ZipCrypto tests). Any pre-existing failure here means
STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 2):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS (byte-identical JSON across all archives)` and
`[cross_read] ... all_match=true` in `results/verify-crossread.err`. Both
unencrypted **and** Phase-1 ZipCrypto paths must remain byte-identical.

---

## 2. New Cargo deps (engineer must add)

Per phase-plan §2 Phase 2:
```bash
cargo add aes cbc ctr hmac sha1 pbkdf2 -p zip-core   # RustCrypto
```
**Expected:** `crates/zip-core/Cargo.toml` gains these RustCrypto crates; the
lockfile updates; the crate still builds (`cargo build -p zip-core`).

---

## 3. TC-1 — Byte-identical read of C-written AES archives (both directions)

**Setup:** `gen_corpus` must additionally emit AES-128 and AES-256 encrypted
archives (e.g. `aes128_enc.zip`, `aes256_enc.zip`), written by C libzip with a
known password, ground-truth inputs mirrored under
`data/corpus-verify/inputs/<aes_name>/<index>`.

**Command (C writes → Rust reads, via `cross_read` part a + `verify_read` diff):**
```bash
bash run-verify.sh
target/release/verify_read.exe libs/c/zip.dll data/corpus-verify > results/verify-read.json
target/release/verify_read.exe target/release/zip.dll data/corpus-verify > results/verify-read-rust.json
diff -u results/verify-read.json results/verify-read-rust.json
target/release/cross_read.exe libs/c/zip.dll data/corpus-verify
```

**Expected:**
- AES archives open in BOTH libraries (no `ZIP_ER_ENCRNOTSUPP`).
- Every entry's `fopen`/`fopen_index` FNV-1a fingerprint + length are
  **byte-identical** between C and Rust (empty `verify-read.diff`).
- `stat.encryption_method` is `257` (AES-128) or `259` (AES-256) in both.
- `cross_read` prints `all_match=true` (SHA-256 + byte compare against
  ground-truth inputs).

**Rust writes → C reads (part b):** `cross_read` part (b) must also write a
Rust-written AES archive (`rust-written/rust_aes256.zip`) with the known
password and have C libzip read it back.
**Expected:** every entry byte-identical (SHA-256), names/sizes match;
`Rust-writes/C-reads all_match=true`.

---

## 4. TC-2 — HMAC-SHA1 integrity check (corrupted ciphertext → `ZIP_ER_CRC`)

**Setup:** take a valid Rust- or C-written AES archive and flip one or more
ciphertext bytes (not the authentication header) on a copy.

**Command:**
```bash
cargo test --package zip-core aes_integrity_corruption
```
(plus a manual probe: corrupt a byte in the ciphertext of an AES entry, reopen,
attempt decrypt.)

**Expected:**
- The corrupted entry fails to read.
- Error is `ZIP_ER_CRC` = `7` (integrity/authentication failure), not a panic
  and not a silent wrong-data return.
- `zip_strerror` returns exactly `"CRC error"`.

---

## 5. TC-3 — Wrong password → `ZIP_ER_WRONGPASS`(27)

**Command:**
```bash
cargo test --package zip-core aes_wrong_password
cargo test --package zip-sys aes_wrong_password
```

**Expected:**
- Opening an AES entry with an incorrect password fails.
- Error code is `ZIP_ER_WRONGPASS` = `27`.
- `zip_strerror` returns exactly `"Wrong password provided"` (byte-identical to C).

---

## 6. TC-4 — No password → `ZIP_ER_NOPASS`(26)

**Command:**
```bash
cargo test --package zip-core aes_no_password
cargo test --package zip-sys aes_no_password
```

**Expected:** opening an AES entry with no password set fails with `ZIP_ER_NOPASS`
= `26` and string `"No password provided"`.

---

## 7. TC-5 — `zip_encryption_method_supported` returns true for AES

**Command:**
```bash
cargo test --package zip-sys encryption_method_supported
```
(extend the existing `zip_encryption_method_supported` test — it currently
asserts NONE and TRAD_PKWARE true, unknown `99` false.)

**Expected:**
- `zip_encryption_method_supported(ZIP_EM_AES_128, 1)` → `1` (true)
- `zip_encryption_method_supported(ZIP_EM_AES_192, 1)` → `1`
- `zip_encryption_method_supported(ZIP_EM_AES_256, 1)` → `1`
- `zip_encryption_method_supported(<unsupported>, 1)` → `0` (e.g. `99`)

---

## 8. TC-6 — `zip_stat` reports `ZIP_EM_AES_128/192/256`

**Command:**
```bash
cargo test --package zip-core stat_encryption_method_aes
cargo test --package zip-sys stat_encryption_method_aes
```

**Expected:**
- AES-128 entry → `encryption_method == 257` (`ZIP_EM_AES_128`)
- AES-192 entry → `258` (`ZIP_EM_AES_192`)
- AES-256 entry → `259` (`ZIP_EM_AES_256`)
- Unencrypted entry still reports `0` (`ZIP_EM_NONE`); ZipCrypto still `1` — no
  regression of Phase 1 / §5 item 3.

---

## 9. TC-7 — No panic on malformed input

**Command:** extend the existing fuzz targets to feed malformed/truncated AES
ciphertext, truncated AES headers, and truncated PBKDF2/SALTs:
```bash
cargo +nightly fuzz run fuzz_entry_reader
cargo +nightly fuzz run fuzz_codec
cargo +nightly fuzz run fuzz_central_dir
```
plus robustness unit tests:
```bash
cargo test --package zip-core aes_malformed_no_panic
```

**Expected:**
- No panic, no abort, no unwrap on `None`/`Err` for: truncated AES local
  header, truncated SALT, truncated 2-byte password-verification value,
  truncated HMAC, truncated ciphertext, and invalid AES key length.
- Malformed input yields a `Result::Err` with a defined code (`ZIP_ER_CRC`,
  `ZIP_ER_TRUNCATED_ZIP`, or `ZIP_ER_WRONGPASS`), never a panic.

---

## 10. New C ABI / API surface required (engineer must export/verify)

- Reuse Phase-1 symbols for AES read/write path: `zip_fopen_encrypted`,
  `zip_fopen_index_encrypted`, `zip_file_set_encryption`,
  `zip_set_default_password`.
- `zip_encryption_method_supported` must now return true for the three AES
  methods (currently a stub). It is already exported.
- AES read/write must be reachable via `zip_core::Archive`/`write_archive` and
  via `zip_file_set_encryption(zh, index, ZIP_EM_AES_256)`.

Verify symbol resolution and round-trip through the C ABI:
```bash
cargo test --package zip-sys aes_round_trip_abi
```
**Expected:** a full C-ABI round-trip — `zip_set_default_password` →
`zip_file_set_encryption(ZIP_EM_AES_256)` → write → close → reopen →
`zip_fopen_encrypted` with the correct password returns the original bytes;
`zip_stat` reports AES-256. All symbols resolve via `libloading`/`dlopen`.

---

## 11. Pass criteria (summary)

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions, incl. Phase 1 tests).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true
   (unencrypted + ZipCrypto paths unchanged).
3. TC-1: C-written AES-128/256 reads byte-identically in Rust; Rust-written AES
   reads byte-identically in C.
4. TC-2: HMAC-SHA1 integrity — corrupted ciphertext → `ZIP_ER_CRC`(7).
5. TC-3/TC-4: WRONGPASS(27) and NOPASS(26) with exact C strings.
6. TC-5: `zip_encryption_method_supported` true for AES-128/192/256.
7. TC-6: `zip_stat` reports `ZIP_EM_AES_128/192/256` (257/258/259); unencrypted
   still `0`.
8. TC-7: no panic on malformed AES input (fuzz + unit).
9. C-ABI round-trip (`aes_round_trip_abi`) passes; all Phase-1 + AES symbols
   resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

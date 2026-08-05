# Phase 3 — Write-path metadata: comments, extra fields, mtime, attributes, compression — Test Cases

**Phase:** 3 · **Priority:** Medium · **§5 items:** 8 (remainder), 10
**Engineer:** `senior-engineer` · **Tester:** `testing`
**Source of acceptance criteria:** `results/phase-plan.md` §2 Phase 3.
**Depends on:** Phases 1 & 2 (encryption) — DONE (`27cdd73`, `c647936`).
**Constraint:** `zip-core` core logic may be modified only where Phase 3 requires
it. Re-run `run-verify.sh` after the phase (plan §3.5) to prove no read-path
regression.

---

## 0. Current-state reconciliation (READ FIRST)

- `zip-sys` already exports read-side metadata getters: `zip_get_archive_comment`
  (line ~1700) and `zip_file_get_comment` (line ~1735), plus
  `zip_file_extra_fields_count*` / `zip_file_extra_field_get*`. **Do not
  re-assign these — they are read-side and already present.**
- The **write-side setters are MISSING** and are this phase's deliverable:
  `zip_file_set_comment`, `zip_set_file_comment`, `zip_set_archive_comment`,
  `zip_file_extra_field_set`, `zip_file_extra_field_delete`,
  `zip_file_extra_field_delete_by_id`, `zip_file_set_mtime`,
  `zip_file_set_dostime`, `zip_file_set_external_attributes`,
  `zip_file_get_external_attributes`, `zip_file_attributes_init`,
  `zip_set_file_compression`.
- In `zip-core`, `ArchiveFile` (in `compress.rs`) currently carries only `name`
  and `data`; `CompressOptions` handles method/level/parallel/encryption but not
  per-entry metadata. Phase 3 must extend the writer (`compress.rs`),
  serializer (`dirent.rs`), and setters (`archive.rs`).

---

## 1. Build & baseline gate (run first)

```bash
cd C:/Users/kutay/Desktop/Projects/LibzipInRust
cargo build --release --package differential --package zip-sys
cargo test --workspace
```

**Expected:** clean build; all existing tests pass (incl. Phase 1/2 encryption
tests). Any pre-existing failure here means STOP and report to the engineer.

**Regression gate (must stay PASS after Phase 3):**
```bash
bash run-verify.sh
```
**Expected:** `READ-PATH: PASS (byte-identical JSON across all archives)` and
`[cross_read] ... all_match=true` in `results/verify-crossread.err`. Unencrypted
+ ZipCrypto + AES paths must remain byte-identical.

---

## 2. TC-1 — Write→read round-trip preserves archive/file comments

**Command:**
```bash
cargo test --package zip-sys comments_round_trip
```

**Expected:**
- `zip_set_archive_comment(zh, "archive comment", 16)` then write/close/reopen
  → `zip_get_archive_comment` returns exactly `"archive comment"` (byte-for-byte,
  with length `16`).
- `zip_file_set_comment(zh, index, "file comment", 12, 0)` then write/close/
  reopen → `zip_file_get_comment` returns exactly `"file comment"` (length 12).
- Setting an empty/`NULL` comment removes it (reverts to none).
- Round-trip preserves the exact bytes (UTF-8 / non-ASCII comment content
  included).

---

## 3. TC-2 — Write→read round-trip preserves extra fields

**Command:**
```bash
cargo test --package zip-sys extra_field_round_trip
```

**Expected:**
- `zip_file_extra_field_set(zh, index, 0xCAFE, 0, data, len, ZIP_FL_ENC_UTF_8)`
  then write/close/reopen → `zip_file_extra_field_get` (or `_get_by_id`) returns
  the identical `id=0xCAFE`, same data bytes, same length.
- `zip_file_extra_fields_count(zh, index, 0)` reflects the added field.
- Data survives verbatim (binary bytes, not just text).

---

## 4. TC-3 — Write→read round-trip preserves mtime / dos time

**Command:**
```bash
cargo test --package zip-sys mtime_round_trip
```

**Expected:**
- `zip_file_set_mtime(zh, index, unix_time, 0)` then write/close/reopen →
  `zip_stat` reports the same `mtime` (timezone-aware, matching libzip's
  `localtime`/`mktime` semantics — see verification-report §2.2 gap #4, already
  fixed).
- `zip_file_set_dostime(zh, index, dostime, 0)` round-trips the DOS timestamp
  correctly and reads back via `zip_stat`.

---

## 5. TC-4 — Write→read round-trip preserves external attributes

**Command:**
```bash
cargo test --package zip-sys external_attributes_round_trip
```

**Expected:**
- `zip_file_set_external_attributes(zh, index, ZIP_OPSYS_UNIX, 0, 0o100644 << 16)`
  then write/close/reopen → `zip_file_get_external_attributes` returns the same
  (opsys, attributes) pair.
- `zip_file_attributes_init(&attrs)` zero-initializes a `zip_file_attributes_t`
  struct used by the getter (ABI layout must match C).

---

## 6. TC-5 — C reads Rust-written metadata identically (cross-read)

**Setup:** extend `cross_read` part (b) so `zip_core` writes an archive carrying
per-entry archive/file comments, extra fields (e.g. id `0xCAFE`), explicit
mtimes, and external attributes (`rust-written/rust_metadata.zip`).

**Command:**
```bash
target/release/cross_read.exe libs/c/zip.dll data/corpus-verify
```

**Expected:** `Rust-writes/C-reads all_match=true` in
`results/verify-crossread.err`; C libzip reads back the Rust-written comments,
extra fields, mtimes, and external attributes **identically** (byte-for-byte).

---

## 7. TC-6 — `zip_set_file_compression` selects Store/Deflate per entry, byte-identical to C

**Command:**
```bash
cargo test --package zip-sys compression_method_selection
```
plus a differential byte-compare of the on-disk output:
```bash
bash run-verify.sh   # includes a rust-written mixed-method archive in the corpus
```

**Expected:**
- `zip_set_file_compression(zh, index, ZIP_CM_STORE, 0)` → entry stored
  (uncompressed), `comp_method==0`.
- `zip_set_file_compression(zh, index, ZIP_CM_DEFLATE, 9)` → entry deflated,
  `comp_method==8`.
- Mixing per-entry Store and Deflate in one archive produces a Rust-written
  archive whose bytes are **byte-identical** to the equivalent C libzip output
  for the same settings and inputs (diff the outputs).
- Unsupported method returns an error (e.g. `ZIP_ER_COMPNOTSUPP`).

---

## 8. TC-7 — `zip_file_extra_field_set` / `delete` round-trip

**Command:**
```bash
cargo test --package zip-sys extra_field_delete_round_trip
```

**Expected:**
- After `zip_file_extra_field_set`, `count` increments; after
  `zip_file_extra_field_delete` (by index) and
  `zip_file_extra_field_delete_by_id`, `zip_file_extra_fields_count` reflects
  the removals (returns to original value).
- After write/close/reopen the deletions persist (deleted fields are gone from
  the on-disk extra field).
- Deleting a nonexistent id returns the correct libzip error (non-zero, no
  panic).

---

## 9. TC-8 — No panic on malformed metadata input

**Command:** fuzz the metadata parse/write path:
```bash
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_entry_reader
```
plus robustness unit tests:
```bash
cargo test --package zip-core metadata_malformed_no_panic
```

**Expected:** no panic, no abort, no unwrap on `None`/`Err` for: oversized extra
fields, truncated extra-field data, oversized/oversized-length comments, and
invalid `zip_file_attributes_t` pointers. Malformed input yields a defined error
code (`ZIP_ER_INCONS`, `ZIP_ER_EF_TOO_LARGE`=36, etc.), never a panic.

---

## 10. New C ABI symbols required (engineer must export)

`zip_file_set_comment`, `zip_set_file_comment`, `zip_set_archive_comment`,
`zip_file_extra_field_set`, `zip_file_extra_field_delete`,
`zip_file_extra_field_delete_by_id`, `zip_file_set_mtime`,
`zip_file_set_dostime`, `zip_file_set_external_attributes`,
`zip_file_get_external_attributes`, `zip_file_attributes_init`,
`zip_set_file_compression`.

Verify all resolve:
```bash
cargo test --package zip-sys abi_symbols_present
```
**Expected:** all 12 new symbols resolve via `libloading`/`dlopen`; the ABI test
passes.

---

## 11. Pass criteria (summary)

All of the following must hold to hand off to `devops`:

1. `cargo test --workspace` green (no regressions, incl. Phase 1/2 tests).
2. `bash run-verify.sh` → READ-PATH PASS + cross_read all_match=true.
3. TC-1: archive + file comments round-trip byte-exact.
4. TC-2: extra fields round-trip byte-exact (incl. `count`).
5. TC-3: mtime and dos time round-trip (timezone-aware).
6. TC-4: external attributes round-trip; `zip_file_attributes_init` works.
7. TC-5: C reads Rust-written metadata identically (cross-read).
8. TC-6: `zip_set_file_compression` Store/Deflate per entry, byte-identical to C.
9. TC-7: extra-field set/delete round-trip; counts reflect changes.
10. TC-8: no panic on malformed metadata (fuzz + unit).
11. All 12 new C ABI symbols resolve.

On any FAIL, `testing` returns the phase to `senior-engineer` (orchestration
loop step 5).

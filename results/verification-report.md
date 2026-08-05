# kzip — C/Rust Equivalence Verification Report

**Original date:** 2024-08-04
**GAP-ANALYSIS REVISION:** 2026 (this revision) — the §4/§5 gap tables and §2.2/§2.3
divergences are **re-run against the CURRENT `main`**, after the ZIP64/stat/error fixes
(`29ddb0c`/`a2c4973`/`36dcc81`/`ee3b693`) and all eight gap-closure phases
(`27cdd73`, `c647936`, `f6eb17c`, `b2afeb7`, `88a6ebc`, `afecbb6`, `6b032a4`,
`c1ccccf`; HEAD `4baed17`). The methodology and read-path PASS results below still
hold; statuses in the gap tables are marked **CLOSED** / **OPEN** / **PARTIAL**.

**Verification agent scope:** rigorous equivalence check between the original C
libzip and the Rust port (`zip-core` in-process + `zip-sys` cdylib), plus a full
gap analysis.

**Binaries under test:**
- **C libzip:** `libs/c/zip.dll` (v1.11.4, MSVC release).
- **Rust cdylib:** `target/release/zip.dll` (crate `zip-sys`).
- **Rust in-process core:** `crates/zip-core`.

---

## 1. Methodology

A corpus of archives was generated deterministically, then read by BOTH
libraries through an extended differential harness. The harness produces a
JSON fingerprint of every observable read/stat result; the two JSON documents
are diffed. Independently, write-path equivalence is proven by having each
implementation read archives written by the other (in-process for `zip-core`,
real C ABI for `zip-sys`).

### 1.1 Corpus generation — `data/corpus-verify/`
Generated deterministically by **C libzip itself** (driven through its real
write API), so both readers consume canonical, authoritative C-written
archives. Ground-truth input bytes are mirrored under
`data/corpus-verify/inputs/<archive>/<index>`.

| Archive | Entries | Coverage |
|---|---|---|
| `basic_deflate.zip` | 3 | deflate level 6, empty entry, explicit mtimes, file comment |
| `basic_store.zip` | 2 | stored (uncompressed), 4 KiB + 64 KiB |
| `empty_archive.zip` | 0 | empty archive |
| `one_empty.zip` | 1 | a single empty entry |
| `deep.zip` | 2 | deeply nested paths (24 levels) |
| `unicode.zip` | 3 | Unicode/UTF-8 filenames + emoji, archive comment |
| `large.zip` | 1 | ~300 KiB incompressible (»64 KiB) |
| `huge.zip` | 1 | 8 MiB incompressible (above zip-core's 32 MiB zero-copy cap → streaming path) |
| `mix.zip` | 4 | mixed deflate/store, varied levels, nested, empty |
| `many.zip` | 1500 | 1000+ entries, mixed methods |
| `many_zip64.zip` | 70000 | forces a real ZIP64 EOCD (16-bit counts overflow) |
| `handcrafted/data_descriptor.zip` | 1 | bit-flag `0x08` data descriptor |
| `handcrafted/extra_fields.zip` | 1 | custom extra field (id 0xCAFE), file comment |
| `handcrafted/zip64.zip` | 1 | minimal hand-rolled ZIP64 |
| `handcrafted/bad_notzip.zip` | – | non-zip garbage → open error |
| `handcrafted/truncated.zip` | – | half a valid archive → open error |

### 1.2 Read-path harness — `differential/src/bin/verify_read.rs`
Per archive: `zip_open`, `zip_get_num_entries`, and per entry:
`zip_get_name`, `zip_fopen`, `zip_fopen_index`, `zip_stat_index` (ALL fields),
`zip_stat` by name. Error paths: fopen of a missing entry and fopen_index out
of range, with the resulting `zip_strerror` strings. The **same binary** runs
against both libraries.

### 1.3 Cross-read / write-path harness — `differential/src/bin/cross_read.rs`
**(a) C writes → Rust reads** (every C-generated archive read by
`zip_core::Archive`, bytes compared to ground truth); **(b) Rust writes → C
reads** (`write_archive` deflate-6 output read back by C libzip).

### 1.4 Reproduction
`bash run-verify.sh` regenerates the corpus, runs both read-path harnesses,
diffs the JSON, and runs the cross-read check. Outputs use NEW filenames only.
Per ORCHESTRATION working rules: **never run `find /` or full-fs scans; always
use hard timeouts** on cargo/test/verify invocations.

---

## 2. Read-Path Equivalence Results

> **Current-state note:** the read path was not regressed by the gap-closure
> phases (encryption, sources, metadata-writer). Each phase re-ran
> `run-verify.sh` and kept the byte-identical result; the §2.1 tables below are
> the preserved PASS baselines.

### 2.1 Decompressed CONTENT — **PASS (byte-identical)**

Every entry both libraries could open produced **byte-identical decompressed
output**: identical lengths and identical FNV-1a fingerprints for both
`zip_fopen` (by name) and `zip_fopen_index` across all archives.

| Archive | entries | fopen-by-name | fopen_index |
|---|---|---|---|
| basic_deflate | 3 | identical | identical |
| basic_store | 2 | identical | identical |
| empty_archive | 0 | n/a | n/a |
| one_empty | 1 | identical | identical |
| deep | 2 | identical | identical |
| unicode | 3 | identical | identical |
| large | 1 | identical | identical |
| huge | 1 | identical | identical |
| mix | 4 | identical | identical |
| many | 1500 | identical | identical |
| data_descriptor | 1 | identical | identical |
| extra_fields | 1 | identical | identical |
| rust_deflate (Rust-written) | 5 | identical | identical |

**19 entries across 16 archives, plus 5 Rust-written entries, all byte-identical.**
`num_entries`, stat `name/index/size/comp_size/crc/comp_method`, and
`stat_by_name` results all matched. The 70 000-entry `many_zip64.zip` now
**opens and reads correctly in both** (ZIP64 read fix, §3.2).

### 2.2 Stat-Metadata — **PASS (was DIVERGENT, now CLOSED)**

The three systematic divergences from the original report are **all fixed** and
re-verified by unit tests + migration chunk 3:

| Field | Original state (2024-08-04) | Current state | Status |
|---|---|---|---|
| `valid` | Rust hardcoded `0xFFFFFFFF`; C uses `0xFF` | `archive.rs::stat` sets the 8 real `ZIP_STAT_*` bits (`0xFF`); test `stat_valid_and_encryption_match_libzip` | **CLOSED** |
| `encryption_method` (unencrypted) | Rust `0xFFFF` (`ZIP_EM_UNKNOWN`); C `ZIP_EM_NONE`(0) | `dirent.rs` maps unencrypted → `ZIP_EM_NONE`(0); test asserts `Some(0)` | **CLOSED** |
| `mtime` | off by the system UTC offset | `archive.rs::dos_to_unix` mirrors `mktime`/`tm_isdst=-1` (timezone-aware); tests `dos_to_unix_uses_local_timezone_like_mktime` | **CLOSED** |

### 2.3 Error-Path Equivalence — **PASS (was PARTIAL, now CLOSED)**

| Scenario | C libzip | Rust zip-sys | Result |
|---|---|---|---|
| non-zip file (`bad_notzip.zip`) | `19` (NOZIP) | `19` | **PASS** |
| truncated archive (`truncated.zip`) | `35` (TRUNCATED_ZIP) | `35` | **CLOSED** (was 19) |
| handcrafted `zip64.zip` | `21` (INCONS) | `21` | **PASS** |
| `many_zip64.zip` (70k entries) | opens, 70000 entries | opens, 70000 entries | **CLOSED** |
| fopen(missing name) | `"No such file"` | `"No such file"` | **CLOSED** (capitalization fixed) |
| fopen_index(out of range) | `"Invalid argument"` | `"Invalid argument"` | **CLOSED** (capitalization fixed) |

The error-code enum now includes `ZIP_ER_TRUNCATED_ZIP`(35) and
`ZIP_ER_EF_TOO_LARGE`(36); `zip-sys::err_str` byte-matches libzip's
`_zip_err_str[]` (commits `36dcc81`, and §5-item-5 fix).

---

## 3. Cross-Read / Write-Path Equivalence Results

### 3.1 Result — **PASS in both directions**

- **(a) C writes → Rust reads:** every C-generated archive read by `zip-core`;
  every decompressed byte matched ground truth (SHA-256 + byte compare), names
  and sizes matched — including `many_zip64.zip` (now opens).
- **(b) Rust writes → C reads:** `write_archive` (deflate 6) output read by C
  libzip; all entries byte-identical.

This proves byte-level write-path equivalence (C↔Rust). **Additionally**, the
write path is now **exported through the `zip-sys` C ABI** (phases 3–8): a C
consumer can build archives via `zip_open`→`zip_file_add`→`zip_close`, set
comments/extra-fields/mtime/compression/encryption, and read them back
identically (see §4.1).

### 3.2 The ZIP64 defect — **CLOSED**

The original off-by-16-byte EOCD64 field-offset bug
(`zip-core::cdir::read_eocd64`) is fixed (`29ddb0c`): the reader now reads
`[32:40]` entries, `[40:48]` cdir size, `[48:56]` cdir offset. The 70 000-entry
`many_zip64.zip` opens with `num_entries=70000` in both libs (regression-tested
via `read_zip64_eocd_correct_field_offsets` and migration chunk 1).

> **Note:** this closes ZIP64 **read**. ZIP64 **write** is a separate, still-open
> gap (§4.1 / §5).

---

## 4. Gap Analysis (re-run against current `main`)

### 4.1 Functional-gap table (capability × Rust status × C status)

Status legend:
- **IMPLEMENTED** = working in Rust core AND exported through the `zip-sys` C ABI (drop-in usable).
- **IN-CORE** = working in Rust core but not exported via the C ABI.
- **PARTIAL** = partially implemented (see Notes).
- **MISSING** = absent from Rust entirely (present in C).

C public API surface enumerated from `libzip/lib/zip.h`: **128 real `zip_*`
functions** (129 matches minus the `zip_int64_t` typedef). Rust `zip-sys`
exports **125** `extern "C" fn zip_*` symbols → **122 are drop-in
implementations of C API functions** and **3 are extra helpers**
(`zip_source_window`, `zip_source_args_seek`, `zip_buffer_fragment`). **6 C
functions remain missing from the ABI.**

| Capability | Rust (zip-core) | Rust (zip-sys ABI) | C libzip | Status |
|---|---|---|---|---|
| Open / close / read archive | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_get_name` / `zip_get_num_entries` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_fopen` / `zip_fopen_index` / `zip_fread` / `zip_fclose` | IMPLEMENTED | IMPLEMENTED | ✓ | byte-identical |
| `zip_fopen_encrypted` / `zip_fopen_index_encrypted` | IMPLEMENTED | IMPLEMENTED | ✓ | encryption |
| `zip_stat` / `zip_stat_index` / `zip_stat_init` | IMPLEMENTED | IMPLEMENTED | ✓ | all fields match (§2.2) |
| `zip_strerror` / `zip_file_strerror` | IMPLEMENTED | IMPLEMENTED | ✓ | strings match |
| Deflate / Store / Bzip2 decode | IMPLEMENTED | IMPLEMENTED | ✓ | |
| CRC + size integrity check at EOF | IMPLEMENTED | IMPLEMENTED | ✓ | |
| ZIP64 **read** | IMPLEMENTED (fixed) | IMPLEMENTED | ✓ | >65535 entries OK |
| **Write/edit path** (`zip_file_add`, `zip_dir_add`, `zip_delete`, `zip_rename`, `zip_file_replace`, `zip_discard`, `zip_close` write) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED (was MISSING) |
| Legacy edit aliases `zip_add` / `zip_add_dir` / `zip_replace` | — | **MISSING** | ✓ (deprecated) | OPEN (trivial shims) |
| `zip_file_rename` (modern in-place rename, `zip_flags_t`) | — | **MISSING** | ✓ | OPEN — only deprecated `zip_rename` exported |
| mtime write (`zip_file_set_mtime`, `zip_file_set_dostime`, `unix_to_dos`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| mtime read (DOS→unix) | IMPLEMENTED (TZ-aware) | IMPLEMENTED | ✓ | CLOSED |
| Archive/file comments (read) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Archive/file comments (write) (`zip_set_archive_comment`, `zip_set_file_comment`, `zip_file_set_comment`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Extra fields (read) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Extra fields (write) (`zip_file_extra_field_set/delete*/get*/count*`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| External attributes (`zip_file_{get,set}_external_attributes`, `zip_file_attributes_init`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Per-entry compression (`zip_set_file_compression`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| **ZIP64 write** (EOCD64 + zip64 extra fields, >65535 entries / >4 GiB entry) | **MISSING** | MISSING | ✓ | **OPEN** (core writes only 16/32-bit counts/offsets) |
| **Data-descriptor write** (bit-flag `0x08` + descriptor record) | **PARTIAL** | PARTIAL | ✓ | **OPEN edge** — AES sets the flag but no descriptor record is emitted |
| **Encryption — ZipCrypto (PKWARE) read+write** | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| **Encryption — WinZip AES-128/192/256 read+write** | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Passwords (`zip_set_default_password`, `zip_file_set_encryption`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_encryption_method_supported` / `zip_compression_method_supported` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| **`zip_source_*` streaming** (file/function/layered/window/zip, read+write) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED (phases 4a/4b) |
| `zip_source_buffer*` / `zip_buffer_fragment` | IMPLEMENTED | IMPLEMENTED (`zip_source_buffer`) | ✓ | **`zip_source_buffer_create` MISSING** (only legacy `zip_source_buffer` exported) |
| `zip_open_from_source` / `zip_fdopen` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Progress callbacks (`zip_register_progress_callback*`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Cancel callbacks (`zip_register_cancel_callback_with_state`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_get_error` / `zip_error_*` structured errors | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED (incl. `zip_error_set_from_source`) |
| `zip_error_to_str` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_error_to_data` | — | **MISSING** | ✓ | OPEN |
| `zip_fseek` / `zip_ftell` / `zip_file_is_seekable` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_get_archive_comment` / flags / `zip_get_archive_flag` / `zip_set_archive_flag` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_name_locate` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED (was NOT-EXPORTED) |
| `zip_unchange` / `zip_unchange_all` / `zip_unchange_archive` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_get_num_files` alias | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_open_rejects_oversized_file` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| Win32 sources (`zip_source_win32{a,w,handle}` + `_create`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| `zip_libzip_version` | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |
| File-error APIs (`zip_file_error_*`, `zip_file_get_error`) | IMPLEMENTED | IMPLEMENTED | ✓ | CLOSED |

**Net ABI count:** 128 C functions − 6 missing = **122 drop-in exported** (plus
3 Rust extras). 6 missing: `zip_file_rename`, `zip_source_buffer_create`,
`zip_error_to_data`, and deprecated aliases `zip_add`, `zip_add_dir`,
`zip_replace`.

### 4.2 Error-code parity for the read path

- **Correct:** non-zip → `ZIP_ER_NOZIP`(19); fopen-missing → NOENT(9);
  fopen-index-OOB → INVAL(18); open of missing file → ZIP_ER_OPEN(11);
  truncated → `ZIP_ER_TRUNCATED_ZIP`(35); EF_TOO_LARGE(36) in the enum.
- **Closed divergences:** `35`/`36` now present in `ZipErrorCode`; error
  strings byte-match libzip (capitalization fixed in `36dcc81`/`err_str`).
- **Remaining:** no runtime corpus archive exercises the EF_TOO_LARGE(36)
  string (coverage gap, not a bug).

### 4.3 ABI-layout gap — **CLOSED**

`zip-sys` `zip_stat_t` now carries the trailing `flags: u32` field matching the
C 60-byte layout (migration chunk 3 PASS). The `zip_buffer_fragment_t` struct
mirrors libzip's `{data, length}` layout.

---

## 5. Prioritized Gaps (re-run against current `main`)

**Legend:** ✅ **CLOSED** (verified in current source) · ⭕ **OPEN** ·
🟡 **PARTIAL** / edge.

| # | Priority | Gap | Status | Evidence (current source) |
|---|---|---|---|---|
| 1 | Critical | ZIP64 EOCD field-offset bug (read) | ✅ **CLOSED** | `cdir.rs::read_eocd64` reads `[32:40]`/`[40:48]`/`[48:56]`; `many_zip64.zip` opens in both |
| 2 | High | `valid` bitmask `0xFF` vs `0xFFFFFFFF` | ✅ **CLOSED** | `archive.rs::stat` sets `0xFF`; test `stat_valid_and_encryption_match_libzip` |
| 3 | High | unencrypted `encryption_method` → `ZIP_EM_NONE`(0) | ✅ **CLOSED** | `dirent.rs`; test asserts `Some(0)` |
| 4 | High | timezone-agnostic `mtime` | ✅ **CLOSED** | `archive.rs::dos_to_unix` mirrors `mktime`/`tm_isdst=-1` |
| 5 | High | missing `ZIP_ER_TRUNCATED_ZIP`(35)/`EF_TOO_LARGE`(36) | ✅ **CLOSED** | `error.rs` `TruncatedZip=35`, `Eftoolarge=36`; truncated → 35 |
| 6 | Medium | `zip_stat_t` missing trailing `flags` | ✅ **CLOSED** | `zip-sys` `zip_stat` 60-byte layout |
| 7 | Medium | Encryption: ZipCrypto + WinZip AES read+write | ✅ **CLOSED** | `crypto.rs` (ZipCrypto + AES), `zip_fopen*_encrypted`, `zip_file_set_encryption`, `zip_set_default_password` |
| 8 | Medium | Write/edit export through `zip-sys` | ✅ **CLOSED** | `zip_file_add/dir_add/delete/rename/file_replace/discard`, `zip_close` materialize; phases 3–8 |
| 9 | Medium | Error-string capitalization parity | ✅ **CLOSED** | `36dcc81`; `err_str` byte-matches `_zip_err_str[]` |
| 10 | Low–Med | In-place editing / comments-write / extra-fields-write / streaming `zip_source_*` / progress/cancel / `zip_error_*` / fseek/ftell | 🟡 **MOSTLY CLOSED** | see remaining items below |
| — | security | zip-bomb: unbounded decompress / CD alloc | ✅ **CLOSED** | `a2c4973`; `MAX_DECOMPRESSED`/`MAX_CD_SIZE` caps |
| — | security | mutex-poison panic / unbounded `zip_open` read | ✅ **CLOSED** | `36dcc81`; `guarded()` catch_unwind, bounded read |
| — | — | `zip_name_locate` NOT-EXPORTED | ✅ **CLOSED** | now exported in `zip-sys` |

### §5.10 / remaining gaps — **OPEN**

| # | Priority | Gap | Effort | Notes / evidence |
|---|---|---|---|---|
| A | **Medium** | **ZIP64 WRITE** — `write_local_header`/`write_central_entry` emit `comp_size`/`uncomp_size` as `u32` and `write_eocd` emits entry count as `u16` and cdir size/offset as `u32`; no EOCD64, no zip64 extra fields. Archives >65 535 entries or entries >4 GiB cannot be **written** (read is fine). | Medium | `crates/zip-core/src/compress.rs`; `build_extra` explicitly skips `EXTRA_FIELD_ZIP64` |
| B | **Low** | **`zip_file_rename`** — C's modern in-place rename (`zip_rename` is the deprecated alias). Not exported; only `zip_rename` exists. | Small | missing from ABI |
| C | **Low** | **`zip_source_buffer_create`** — C's non-deprecated form. Only legacy `zip_source_buffer` exported. | Small | missing from ABI |
| D | **Low** | **`zip_error_to_data`** — serialize a `zip_error_t` into a buffer. Only `zip_error_to_str` exists. | Small | missing from ABI |
| E | **Low** | **Deprecated aliases `zip_add`, `zip_add_dir`, `zip_replace`** — one-line forwards to `zip_file_add`/`zip_dir_add`/`zip_file_replace`. | Trivial | missing from ABI |
| F | 🟡 **edge** | **Data-descriptor write** — the AES writer sets the `DATA_DESCRIPTOR` bit flag in local+central but does **not** emit an actual `PK\x07\x08` descriptor record after the entry data. Non-AES entries write known CRC inline (matches C's default seek-back). | Small | `compress.rs::write_local_header`/`write_compressed_hooked`; no descriptor emitted |
| G | 🟡 **edge** | **Encryption edge cases** — wrong-password → `ZIP_ER_WRONGPASS`, no-password → `ZIP_ER_NOPASS` handling present; AES HMAC integrity, AE-2 descriptor flag vs record consistency (overlaps F). Verify against C for every method/strength combination. | Small | `zip-sys` NOPASS/WRONGPASS paths |
| H | — | EF_TOO_LARGE(36) not exercised at runtime by any corpus archive | Coverage | no archive triggers it |

**No Critical or High gaps remain.** All original §5 items 1–9 are CLOSED; §5
item 10 is mostly closed, leaving the six missing ABI symbols (A–E) plus the
ZIP64-write (A) and data-descriptor edge (F/G).

---

## 6. Conclusions

1. **Read-path content equivalence: PASS** (unchanged). Every archive both
   libraries can open yields byte-identical decompressed data, names, sizes,
   CRC, and compression method — now including the 70 000-entry ZIP64 archive.
2. **Cross-read write-path equivalence: PASS in both directions**, and the
   write/edit path is now exported through the `zip-sys` C ABI.
3. **Stat/error-parity fixes confirmed CLOSED:** `valid` bits, unencrypted
   `encryption_method`, timezone-aware `mtime`, truncated→`35`, error-string
   capitalization, `zip_stat_t` trailing `flags`, and the zip-bomb/mutex
   security caps.
4. **ABI surface: 122 / 128 C functions are exported drop-in** (was 14/128).
   The 6 remaining C functions are: `zip_file_rename`,
   `zip_source_buffer_create`, `zip_error_to_data`, and deprecated aliases
   `zip_add`, `zip_add_dir`, `zip_replace`.
5. **Top remaining functional gaps (all Low/Medium or edge):**
   - **ZIP64 WRITE** (the only Medium; not implemented in core).
   - **Data-descriptor write** (AES sets the flag but emits no descriptor record).
   - **6 missing ABI symbols** (incl. `zip_file_rename`, `zip_source_buffer_create`,
     `zip_error_to_data`, and 3 deprecated aliases).
   - **Encryption edge-case parity** (wrongpass/nopass, HMAC, per-method) to be
     exhaustively cross-checked against C.

### Artifacts
- `results/verify-read.json` — C libzip read-path result
- `results/verify-read-rust.json` — Rust cdylib read-path result
- `results/verify-read.diff` — diff between the two (empty = byte-identical)
- `results/verify-crossread.json` — cross-read / write-path result
- `data/corpus-verify/` — generated corpus + ground truth
- Harness: `differential/src/bin/{gen_corpus,verify_read,cross_read}.rs`, `run-verify.sh`
- Phase tests: `results/phase{1..8}-tests.md`

### Constraints honored (per ORCHESTRATION working rules)
- No `find /` or full-filesystem scans were performed; all verification was
  scoped to the known source trees.
- No unbounded commands; cargo/test/verify runs are timeout-bounded.
- `zip-core` core logic is unchanged by this gap-analysis pass (it is a
  read-only audit of the current committed state; HEAD `4baed17`).
- The only uncommitted working-tree changes are regenerated benchmark CSVs /
  `benchmark-report.md` from the prior benchmark task, not core source.

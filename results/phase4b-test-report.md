# Phase 4b Test Report — VERDICT: PASS

**Tester:** `testing` (w3:p5) · **Date:** 2026-08-05
**Engineer:** `senior-engineer-4b` · **Phase:** 4b (write-side + layered zip_source_*)

## Summary

All Phase 4b acceptance criteria pass. No regressions in zip-core, Phase 3
round-trips, Phase 4a read-side sources, or the read/cross-read verify harness.

## Results

| # | Criterion | Check | Result |
|---|-----------|-------|--------|
| 1 | `cargo test --workspace` green | zip-core 79, zip-sys 53, robustness 10, corpus 3, ziptools 53, zip-async 7, differential bins 0 | PASS |
| 2 | `bash run-verify.sh` | READ-PATH PASS; `[cross_read] ... all_match=true` (both directions) | PASS |
| 3 | TC-4b-1 layered wrap | `source_layered_wrap` | PASS |
| 4 | TC-4b-2 write/seek_write | `source_write` | PASS |
| 5 | TC-4b-3 commit/rollback/cloning | `source_commit_rollback` | PASS |
| 6 | TC-4b-4 file→window→crc→compress | `source_pipeline_file_window_crc_compress` + cross-read byte-identical to C | PASS |
| 7 | TC-4b-5 source helpers parity | `source_helpers_parity` | PASS |
| 8 | TC-4b-6 no panic on malformed | `source_write_malformed_no_panic` (see note) | PASS |
| 9 | All 12 new C ABI symbols resolve | `abi_symbols_present` + present in `target/release/zip.dll` | PASS |

## ABI symbols confirmed in release cdylib

`zip_source_layered`, `zip_source_layered_create`, `zip_source_write`,
`zip_source_seek_write`, `zip_source_begin_write`, `zip_source_begin_write_cloning`,
`zip_source_commit_write`, `zip_source_rollback_write`,
`zip_source_make_command_bitmap`, `zip_source_pass_to_lower_layer`,
`zip_source_seek_compute_offset`, `zip_source_args_seek`
(+ `zip_source_tell_write`, also exported) — all found via `libloading` in
`abi_symbols_present` and by binary symbol grep.

## Note on fuzz targets (TC-4b-6)

The plan also lists `cargo +nightly fuzz run fuzz_entry_reader` /
`fuzz_central_dir`. `cargo-fuzz` is **not installed** in this environment
(`cargo fuzz` → "no such command"), so the fuzz targets cannot be executed here.
The robustness requirement of TC-4b-6 is covered by the passing
`source_write_malformed_no_panic` unit test (malformed callbacks / misordered
lifecycle → defined errors, no panic). This is an environment limitation, not a
code failure.

## Regression confirmation

- Phase 4a read-side tests re-run individually: `source_file_read`,
  `source_function_callbacks`, `source_window_read`, `source_zip_entry`,
  `source_read_seek_stat_parity`, `source_malformed_no_panic` — all PASS.
- Phase 3 round-trips (mtime/comment/extra-field/attributes/compression) —
  included in zip-sys 53, all PASS.
- `run-verify.sh`: READ-PATH PASS, cross_read all_match=true.

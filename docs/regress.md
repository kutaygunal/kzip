# libzip regress mapping

`libzip/regress/` ships **187 `.test` files** from the C project. They are
nihtest/shell-driven and run against the C tools. We do **not** run them
directly; instead we port the high-value scenarios into native Rust integration
tests and verify parity via the differential harness.

## Differential coverage (C vs Rust, byte-identical)

`bash run-differential.sh` exercises the read path over `data/corpus` through
*both* the C `libs/c/zip.dll` and the Rust `target/release/zip.dll`, and diffs
the JSON. This is the strongest equivalence proof for the read subset and
subsumes many regress read cases.

## `run-verify.sh` (full verification vs libzip 1.11.4)

`bash run-verify.sh` is the canonical equivalence driver against C libzip
**1.11.4**. It builds the differential harness + cdylib, generates a deterministic
corpus with the C write API, runs the extended read-path harness against *both*
C `libs/c/zip.dll` and Rust `target/release/zip.dll`, diffs the JSON, and runs the
write-path cross-read check:

- `results/verify-read.json` / `verify-read-rust.json` — C and Rust read results
- `results/verify-read.diff` — the byte-diff (must be empty for a PASS)
- `results/verify-crossread.json` — write-path cross-read result
- `results/verification-report.md` — the full report

Run it after any core change:

```sh
bash run-verify.sh
```

## Ported to native Rust tests

| libzip regress family | Ported test(s) |
|-----------------------|----------------|
| `read_zip`, `read_data`, `read_empty`, `read_nested` (read path) | `crates/zip-core/tests/corpus.rs` (text/nested/binary corpus: open, enumerate, read, length/content asserts) |
| `crash_*`, malformed/truncated input (no-panic posture) | `crates/zip-core/tests/robustness.rs` + `fuzz/` targets |
| FFI open/read/stat round-trip | `crates/zip-sys/src/lib.rs` `#[cfg(test)]` |
| Codec store/deflate decode | `crates/zip-core` codec unit tests + `fuzz_codec` |

## Not yet ported (write/edit/encryption phases)

Regress files that depend on the write path, archive modification, encryption,
or source-construction APIs are deferred until those features land. See
`docs/ABI.md` for the deferred symbol list.

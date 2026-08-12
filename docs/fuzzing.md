# Fuzzing & hardening

## Posture

zip-core targets **no panic on external/malformed input**: parsers and decoders
return `Err`, never panic. This is enforced by:

1. **Robustness tests** (`crates/zip-core/tests/robustness.rs`) — run under the
   normal test harness; feed random / truncated / bit-mutated bytes to
   `Archive::open`, `open_entry`, `Dirent::parse_central`, and
   `decode_slice_into`, asserting no panic.
2. **cargo-fuzz targets** (`fuzz/`) — a standalone cargo-fuzz project
   (libFuzzer). See `fuzz/README.md`. These require `cargo install cargo-fuzz`
   + nightly and are not run in the default CI path.

## Security model

- **No `unsafe` in `zip-core` / `zip-async`** (`#![deny(unsafe_code)]`). Unsafe
is confined to the FFI boundary (`zip-sys`) and test crates.
- **Checked arithmetic** in the parser (reject overflows with `ZipError`).
- **Bounded allocation**: entries and buffers are size-capped where practical.

## Miri

`cargo miri` is installed locally and is wired into CI as an **opt-in** job
(see `.github/workflows/ci.yml`, `miri` job, triggered by the `miri` label).

```sh
cargo +nightly miri test -p zip-core --lib
```

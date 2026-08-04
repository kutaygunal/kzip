# LibzipInRust

A commercial-grade, memory-safe Rust port of [libzip](https://libzip.org/),
featuring:

- **`zip-core`** — a safe, `#![deny(unsafe_code)]` engine for reading, creating,
  and modifying ZIP archives.
- **`zip-async`** — a tokio adapter exposing the engine behind `AsyncRead`.
- **`zip-sys`** — a `#[no_mangle]` C ABI (`cdylib`) that is drop-in compatible
  with libzip's public header for the implemented read-path subset.
- **`ziptools`** — `zipcmp`/`zipmerge`/`ziptool`-style command-line tools.
- **`differential`** — a harness that loads both the original C libzip and the
  Rust cdylib and diffs their JSON behavior on a corpus.
- **`libzip-benches`** — criterion benchmarks for parallel compression scaling
  and zero-copy memory usage.

## Build & test

```sh
cargo build --workspace
cargo test --workspace
```

The `cargo test --workspace` suite includes unit, integration, FFI, and
robustness (no-panic-on-malformed-input) tests.

## C ABI

See [C ABI / FFI status](ABI.md) for the exact symbol subset implemented,
stubbed, and deferred.

## Differential testing

```sh
bash run-differential.sh
```

This builds the Rust workspace, runs the harness against both the C libzip
(`libs/c/zip.dll`) and the Rust cdylib (`target/release/zip.dll`), and diffs the
JSON. A PASS means byte-identical behavior on the read path for the corpus.

## License

BSD-3-Clause. See `PLAN.md` for the original libzip attribution and the
independent-implementation note.

# libzip-fuzz

cargo-fuzz targets for `zip-core`. The posture is **no panic on malformed
input**: parsers and decoders must return `Err`, never panic, on arbitrary or
truncated bytes.

> **Note:** this crate is a standard cargo-fuzz layout and is intentionally
> excluded from the root workspace (it depends on `libfuzzer-sys`, which is not
> installed in the standard environment). The no-panic posture is verified
> locally without cargo-fuzz by the robustness tests in
> `crates/zip-core/tests/robustness.rs`, which run under `cargo test --workspace`.

## Targets

| Target               | What it feeds                              |
|----------------------|--------------------------------------------|
| `fuzz_central_dir`   | `zip_core::cdir::read_central_dir`         |
| `fuzz_entry_reader`  | `Archive::open` + `open_entry` + read      |
| `fuzz_codec`         | `Dirent::parse_central` + `decode_slice_into` (Store/Deflate) |

## Running (requires cargo-fuzz + nightly)

```sh
cargo install cargo-fuzz
rustup component add llvm-tools-preview
# from the fuzz/ directory:
cargo +nightly fuzz run fuzz_central_dir
cargo +nightly fuzz run fuzz_entry_reader
cargo +nightly fuzz run fuzz_codec
```

## Local (no-fuzz) verification

```sh
cargo test -p zip-core --test robustness
```

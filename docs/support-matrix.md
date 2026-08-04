# Support Matrix

What is supported, where, and at what maturity. This is the source of truth for
platforms, MSRV, codecs, and feature flags.

## Crates

| Crate | Purpose | Publishable |
|-------|---------|-------------|
| `zip-core` | Safe core engine (read/write/compress) | yes (see §Publishing) |
| `zip-async` | Async streaming adapter | `publish = false` for now |
| `zip-sys` | C-ABI `cdylib` FFI layer | `publish = false` for now |
| `ziptools` | `zipcmp`/`zipmerge`/`ziptool` equivalents | `publish = false` |
| `libzip-benches` | Criterion + C/Rust serial harnesses | `publish = false` |
| `differential` | C-vs-Rust differential harness | `publish = false` |
| `libzip-fuzz` | cargo-fuzz targets (standalone) | `publish = false` |

## Platforms

Targets exercised in CI (`cargo test --workspace` on `ubuntu-latest`,
`windows-latest`, `macos-latest`):

| OS / arch | Support | Notes |
|-----------|---------|-------|
| x86_64 Linux | ✅ tested | gnu + musl builds are used in deny graph |
| aarch64 Linux | ✅ | cross-target declared in `deny.toml [graph]` |
| x86_64 macOS | ✅ tested | |
| aarch64 macOS | ✅ | |
| x86_64 Windows | ✅ tested (MSVC) | C baseline built with MSVC 2022 |
| Other targets | ⚠️ best-effort | must pass `#![deny(unsafe_code)]` gate (core is portable) |

## MSRV

- **Minimum supported Rust version: 1.75** (`rust-version = "1.75"` in the
  workspace).
- CI runs the current stable toolchain. Raising the MSRV is a deliberate,
  documented decision (see `docs/msrv.md`).

## Codecs

The codec matrix mirrors the C libzip baseline (`results/C-BASELINE.md`). Rust
bindings in `zip-core` track the same capabilities:

| Codec | zip-core | C baseline backend | Status |
|-------|----------|--------------------|--------|
| DEFLATE | `flate2` | zlib (vcpkg) | ✅ on |
| Bzip2 | `bzip2-rs` | vcpkg bzip2 | ✅ on |
| Zstd | — | not built in baseline | ⚠️ planned |
| LZMA / XZ | — | not built in baseline | ⚠️ planned |
| Store | built-in | built-in | ✅ on |
| WinZip AES | zip-core | Windows BCrypt (C) | ✅ on (C); see ABI.md |

> Zstd/LZMA are not yet in the C baseline or the Rust codec layer; enabling them
> is a documented follow-up (see `results/C-BASELINE.md` "Next baseline steps").

## Feature flags

- `zip-core`: `parallel` *(default)* — rayon-based parallel compression with
  deterministic output. Disable for a minimal dependency-light build.
- `zip-async`: default (tokio bridge).
- `zip-sys`: default (cdylib + rlib).

## Performance gates

See `results/phase5-benchmarks.md` for measured numbers vs. the §9.5 acceptance
gates (serial ≥90% of C; parallel ≥1.8×/3×/5× at 2/4/8 workers with
byte-identical output). **Serial gate not yet met** (≈84%); parallel scaling and
determinism are green.

## Publishing

`zip-core` is the only publishable crate at this stage. `publish = false` crates
(`zip-async`, `zip-sys`, `ziptools`, `benches`, `differential`) cannot be pushed
to crates.io as-is; they are intended for internal/workspace use. Plan for 1.0:

- Remove `publish = false` from crates intended for public consumption.
- Keep `zip-sys` private unless shipping the cdylib to a registry is desired.
- See `.cargo/release.toml` and `docs/msrv.md` for the release flow.

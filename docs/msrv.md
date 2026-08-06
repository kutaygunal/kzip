# MSRV & Release Policy

## Minimum Supported Rust Version (MSRV)

`rust-version = "1.75"` (set in `Cargo.toml`). We target the current stable
toolchain in CI and keep the MSRV floor at 1.75.

**Policy:**

- New dependencies or features must not raise the MSRV above 1.75 without an
  explicit, documented decision.
- CI runs on the latest stable; a dedicated MSRV job pins 1.75 and is verified
  locally with `cargo +1.75.0 test --workspace` when a toolchain is available.
- The MSRV is re-evaluated at each release; raising it is a deliberate,
  reviewed change.

## Release Process

Releases are configured via `cargo-release` v1.x. The configuration lives in
the workspace-root **`release.toml`** (read by cargo-release ≥1.0); a pointer is
kept at `.cargo/release.toml`. The pipeline is documented but **not** run from
CI by default (a tag-gated, approval-gated `release` job is in
`.github/workflows/ci.yml`). Manual steps:

1. Ensure `cargo test --workspace`, `cargo build --release --workspace`, and
   `bash run-differential.sh` all pass.
2. Run `cargo +1.75.0 test --workspace` to confirm the MSRV floor.
3. `cargo release` with the version bump policy in `release.toml`:
   `patch` (compatible), `minor` (breaking pre-1.0), `major` (→ 1.0.0).
4. Tag and push; record the C-ABI baseline and performance numbers in
   `results/`.

Release bundles are produced under `release/` (see `release.toml`). On Windows the
artifact is `kzip-<ver>-windows-x86_64.zip` containing `kzip.dll` (the `zip-sys` cdylib),
`kzip.h` (the generated header), and `kzipcmp.exe` (the `ziptools`-built `zipcmp` port).

Only `zip-core` is publishable to crates.io at this stage; the other workspace
crates set `publish = false` (see `docs/support-matrix.md`). Reaching 1.0 means
removing `publish = false` from any crate intended for public consumption and
re-running `cargo release major`.

## Versioning

`0.1.0` pre-1.0: breaking ABI changes are acceptable but must be documented in
`docs/ABI.md` and reflected in `crates/zip-sys/include/zip.h`.

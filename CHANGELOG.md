# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed — licensing

- **License corrected to BSD-3-Clause** (matching the rest of the repo, which
  already declared BSD-3-Clause in `README.md`, `docs/index.md`, `zip-core`
  docs, `fuzz/Cargo.toml`, and `deny.toml`).
  - `LICENSE` is now the BSD-3-Clause text with **both** copyrights preserved:
    libzip's original `Copyright (C) 1999-2025 Dieter Baron and Thomas Klausner`
    (required because the Rust port is derived from the BSD-3-Clause libzip
    project) plus the Rust port author's own `Copyright (C) 2026 Kutay Gunal`.
  - Root `Cargo.toml` `license` set to `"BSD-3-Clause"` (all crates inherit via
    `license.workspace = true`).

### Added — Phase 5 (commercial readiness)

- **Performance acceptance harness:**
  - `benches/src/bin/c_serial.rs` — C libzip serial throughput (MiB/s) via
    `libloading` over the `zip_*` ABI.
  - `benches/src/bin/rust_serial.rs` — Rust `zip-core` serial throughput for the
    same corpus.
  - `benches/src/lib.rs` — shared deterministic mixed benchmark corpus.
  - `scripts/bench-serial.sh` — driver that runs both and prints the Rust/C ratio
    against the ≥90% serial gate.
  - Results recorded in `results/phase5-benchmarks.md`, `results/c-serial.csv`,
    `results/rust-serial.csv`.
- **Security & audit trail:**
  - Rewrote `deny.toml` for `cargo-deny 0.20` (bans/licenses/advisories all
    pass; `cargo audit` reports 0 vulnerabilities).
  - `crates/zip-core/README.md` now generated from the crate doc comment via
    `cargo-rdme` (`.cargo-rdme.toml`; `--check` green).
  - `fuzz/Cargo.toml` declared `license = "BSD-3-Clause"` and `rust-version`.
  - Audit trail in `results/security-audit.md`.
- **Release readiness:**
  - Git repository initialized (`main`).
  - `CHANGELOG.md`, `docs/migration.md`, `docs/support-matrix.md` added.
  - `.cargo/release.toml` completed for `cargo release` (shared version, sign,
    publish-per-crate, pre-1.0 `0.x` policy).
  - Optional release CI job (tag-gated, inert) in `.github/workflows/ci.yml`.

## [0.1.0]

### Added — Phases 0–4

- **Phase 0 — Scaffolding & baselines:**
  - Workspace skeleton, GitHub Actions CI (`fmt`, `clippy`, `test`,
    `docs`, `audit`, `fuzz-smoke`, `deny`, opt-in `miri`).
  - `deny.toml` (bans/licenses/advisories).
  - Built original C libzip v1.11.4 (`libs/c/zip.dll`), baseline recorded in
    `results/c-baseline.json` / `results/C-BASELINE.md`.
  - Differential harness skeleton (`differential/`) using `libloading`.
- **Phase 1 — Core engine (read path):**
  - `zip-core` `error`, `Source` trait, buffer/file/window sources.
  - Central-directory parser/writer, entry table, name/extra-field handling.
  - Codec bindings (DEFLATE via `flate2`, Bzip2 via `bzip2-rs`) + CRC.
  - Serial read + extract; differential read tests against C libzip.
- **Phase 2 — Write & edit path:**
  - `ArchiveWriter`/`write_archive`, entry add/delete/rename, unchange semantics.
  - ZipCrypto + WinZip AES encode/decode.
  - Progress/cancel hooks; differential write/edit tests (byte-identical).
- **Phase 3 — New capabilities:**
  - Parallel compression (rayon) with deterministic, byte-identical output.
  - Zero-copy read path + `BufferPool`.
  - Async adapter crate `zip-async` (bridge).
  - Benchmarks: `parallel` (scaling + determinism), `zerocopy` (memory).
- **Phase 4 — C ABI & hardening:**
  - `zip-sys` `#[no_mangle]` FFI layer + `cbindgen`-generated `zip.h`
    (hand-maintained in `crates/zip-sys/include/zip.h`).
  - Fuzz targets (`fuzz/`), Miri opt-in, sanitizer CI runs.
  - Docs (`mdbook`), MSRV policy (`rust-version = "1.75"`), `cargo-release` config.

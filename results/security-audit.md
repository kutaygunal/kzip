# Security & Audit Trail — Phase 5

Recorded: 2026-08-04
Scope: dependency/license audit, licensing cleanup, and README sync for the
commercial-readiness gate. Maps to `PLAN.md` §10 (Security & Memory-Safety).

## What was checked

### 1. `cargo audit` (RustSec advisory database)
- Tool: `cargo-audit 0.22.2` (installed this session).
- Database: `rustsec/advisory-db`, 1189 advisories loaded.
- Scope: `Cargo.lock` (87 crate dependencies).
- **Result: 0 vulnerabilities** (exit 0).

### 2. `cargo deny` (bans / licenses / advisories)
- Tool: `cargo-deny 0.20.2` (installed this session).
- The checked-in `deny.toml` was written for an older cargo-deny schema and was
  **malformed** (rejected `version = 2`, `multiple-versions = true`,
  `unmaintained = "deny"`). It was rewritten against the 0.20 schema.
- **Result (all three checks pass):**
  - `bans` — **ok** (no duplicate versions). Internal path-only workspace deps
    trigger `wildcards` *warnings* only (see residual risk).
  - `licenses` — **ok**.
  - `advisories` — **ok** (unmaintained/unsound set to `all`, yanked `warn`).

### 3. Licensing cleanup (BSD-3-Clause)
- Workspace root: `license = "BSD-3-Clause"`.
- All crates inherit via `license.workspace = true`:
  `zip-core`, `zip-async`, `zip-sys`, `ziptools`, `differential`.
- **Fixed:** `fuzz/Cargo.toml` had **no license field**; added
  `license = "BSD-3-Clause"` (+ `rust-version = "1.75"`).
- `deny.toml` allow-list trimmed to match licenses actually in the graph
  (MIT, Apache-2.0, BSD-3-Clause, Unicode-3.0, ISC, Zlib, 0BSD, Unlicense).
- **No mismatched license fields remain.**

### 4. README sync (`cargo-rdme`)
- Tool: `cargo-rdme 2.1.0` (installed this session).
- `crates/zip-core/README.md` is now **generated from the crate-level doc
  comment** in `src/lib.rs` via `<!-- cargo-rdme start/end -->` markers.
- Added `.cargo-rdme.toml` with `strip-links = true` so regeneration works on
  **stable only** (no nightly rustdoc toolchain required).
- `cargo rdme --check` passes (exit 0). Verified the generated README matches.
- Doctest in the new lib.rs doc comment **passes** (`cargo test -p zip-core --doc`).
- Command: `(cd crates/zip-core && cargo rdme)` to regenerate.

## Findings
- **No known CVEs** in the current dependency graph (cargo audit: 0).
- No banned / duplicate / unlicensed dependencies (cargo deny: clean).
- License field missing on `libzip-fuzz` — fixed.

## Residual risks (per PLAN §10)
- `wildcards = "warn"` in `deny.toml`: internal path-only workspace deps
  legitimately omit a version. If stricter wildcard enforcement is desired,
  add explicit `version` to each internal path dep instead.
- `unmaintained = "all"` may flag transitive crates maintained "by a single
  person"; reviewed at each `cargo deny` run, currently none flagged.
- The core crate enforces `#![deny(unsafe_code)]` (no `unsafe` in zip-core);
  `unsafe` is confined to the FFI boundary (`zip-sys`, differential harness,
  test harnesses) and `zerocopy` casts, which are audit-verified upstream.
- Cargo-rdme intralinks are stripped in the README (stable-only build); the
  full intra-doc links remain in the crate docs.
- Advisory database freshness: CI `cargo audit` job fetches the live database
  each run.

## Reproduce
```sh
cargo audit
cargo deny check bans
cargo deny check licenses
cargo deny check advisories
(cd crates/zip-core && cargo rdme --check)
```

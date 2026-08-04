# LibzipInRust — Commercial-Grade Rust Port of libzip

**Status:** Planning (Draft v0.1)
**Date:** 2026
**Source baseline:** libzip v1.11.4 (cloned at `./libzip`)
**License:** Original libzip is BSD-3-Clause (C) 1999-2020 Dieter Baron & Thomas Klausner.
The Rust port may be independently implemented under a permissive license (see Legal).

---

## 1. Executive Summary

libzip is a battle-tested C library for reading, creating, and modifying ZIP archives.
It has been hardened over 25+ years and ships a **139-function public API** and a
**187-case regression suite**. This plan describes rewriting it as a **commercial-grade
Rust library** without sacrificing correctness, while adding three capabilities the C
version lacks out of the box:

1. **Parallel compression** — compress independent files in an archive concurrently.
2. **Zero-copy I/O** — avoid copying buffers on the read/decode path.
3. **Async streaming** — non-blocking archive read/write for embedded/network use.

The guiding principle: **correctness and ZIP-format conformance are the #1 commercial
risk**, not raw speed. The port must be **bug-for-bug compatible** with the C version on
every valid/invalid input we can feed it, verified by a differential testing harness.

---

## 2. Goals & Non-Goals

### 2.1 Goals
- A memory-safe, panic-free-on-malformed-input Rust library (`#![no_std]`-optional core).
- Feature-parity with the full libzip API surface, plus an idiomatic Rust layer.
- **Drop-in C ABI** via FFI so existing libzip consumers can relink unchanged.
- Measurable wins in throughput (parallel) and memory footprint (zero-copy).
- CI with unit, integration, differential, property, and fuzz testing.
- A repeatable benchmark suite comparing original (C) vs. new (Rust).

### 2.2 Non-Goals (v1)
- No GUI/tooling beyond `zipcmp`/`zipmerge`/`ziptool` equivalents for conformance.
- No rewrite of the compression codecs themselves (DEFLATE/zstd/xz/bzip2) — we bind
  to mature, independently-verified implementations. Reinventing these is high-risk,
  low-reward.
- No `no_std` in v1 (defer; keep the core platform-independent so it's achievable).

---

## 3. Baseline Architecture Analysis (libzip C)

### 3.1 Core abstraction: `zip_source`
The heart of libzip is the **layered source** model. Every data input is a `zip_source_t`:
a node in a stack, where each layer implements a command switch:

```
zip_source_cmd: SRC_OPEN, SRC_READ, SRC_CLOSE, SRC_SEEK, SRC_TELL,
                SRC_STAT, SRC_SUPPORTS, SRC_WRITE, SRC_BEGIN_WRITE,
                SRC_COMMIT_WRITE, SRC_ROLLBACK_WRITE, SRC_ERROR, SRC_FREE,
                SRC_ACCEPT_EMPTY, SRC_GET_FILE_ATTRIBUTES, SRC_IS_DELETED
```

Layers compose: `file → window → crc → compress → aes/pkware → (write sink)`.
This gives streaming, lazy reads, and in-place archive edits without full re-write.

### 3.2 Module map (144 files in `lib/`)
| Area | Representative files | Responsibility |
|------|---------------------|----------------|
| **Central archive** | `zip_open.c`, `zip_close.c`, `zip_add.c`, `zip_delete.c`, `zip_dirent.c` | Archive lifecycle, central directory, entry management |
| **Sources** | `zip_source_*.c` (30+ files) | The layered data pipeline |
| **Compression** | `zip_algorithm_{deflate,zstd,xz,bzip2}.c` | Codec bridging |
| **Encryption** | `zip_pkware.c`, `zip_winzip_aes.c`, `zip_crypto_*.c` | ZipCrypto, AES |
| **Localization/names** | `zip_name_locate.c`, `zip_string.c`, `zip_utf-8.c` | Name lookup, encoding |
| **Errors** | `zip_error.c`, `zip_error_*.c` | Error model, `strerror` |
| **Metadata** | `zip_extra_field.c`, `zip_stat.c`, comments | Extra fields, stats |
| **Platform I/O** | `zip_source_file_{stdio,win32}.c`, `zip_random_*.c` | File backends, RNG |
| **Utilities** | `zip_hash.c`, `zip_buffer.c`, `zip_memdup.c` | Hash map, ring buffer, helpers |

### 3.3 Error model
libzip distinguishes **zip errors** (format/API) from **system errors** (errno).
`zip_error_t` carries `{zip_error_code, system_error, str}`. Our Rust equivalent must
preserve this two-axis error space for C ABI compatibility and CLI parity.

### 3.4 Concurrency reality of the C code
- **Single-threaded** by default; caller must hold a lock around a `zip_t*`.
- No internal parallelism: compression is serial.
- Some internal state is global/mutable (e.g., registered codec callbacks), limiting
  safe multithreading.

---

## 4. Technology & Dependency Selection

### 4.1 Language & edition
- **Rust 2021 edition**, MSRV pinned to a recent stable (e.g., 1.75+).
- Workspace layout: `crates/*` to keep the core, FFI, and tooling decoupled.

### 4.2 Dependency strategy
| Concern | Chosen crate | Rationale |
|---------|-------------|-----------|
| DEFLATE | `flate2` (miniz_oxide backend) | Pure-Rust fallback + optional zlib-ng native for peak speed |
| Zstandard | `zstd` | Official binding, mature |
| XZ/LZMA | `liblzma` / `xz2` | Wraps system liblzma |
| Bzip2 | `bzip2` | Wraps system libbz2 |
| AES | `aes` + `cbc` + `ctr` (RustCrypto) | Pure-Rust, audited |
| ZipCrypto | hand-written | ~150 LOC, well-documented algorithm |
| CRC32 | `crc32fast` | Hardware-accelerated |
| Hashing | `rustc-hash` for internal maps | Fast, deterministic |
| Async runtime | `tokio` (feature-gated) | De-facto standard; keep core sync |
| Parallelism | `rayon` (feature-gated) | Simple work-stealing for compression |
| Zero-copy | `bytes` / `zerocopy` | Buffer abstractions + safe casts |
| Error | `thiserror` | Idiomatic typed errors |
| Testing | `proptest`, `criterion`, `arbitrary`, `libfuzzer-sys` | Property, bench, fuzz |
| FFI ABI | `libc`, `cbindgen` | C-header generation |
| Docs | `cargo-rdme`, `mdbook` | API docs |

> **Decision:** Compression codecs are bound, **not reimplemented**. This preserves
> format correctness (the hardest part) while our value-add is architecture, not codecs.

---

## 5. High-Level Architecture (Target)

```
┌────────────────────────────────────────────────────────────┐
│                    Idiomatic Rust API                      │
│   Archive / ZipFile / EntryBuilder / ReadStream<Tokio>     │
└────────────────────────────────────────────────────────────┘
   │                        ▲
   ▼                        │
┌────────────────────────────────────────────────────────────┐
│                  Core Engine (sync, std)                   │
│   • CentralDirectoryParser/Writer   • EntryTable           │
│   • ZipSource pipeline (layered)    • Error model          │
│   • Codec registry (deflate/zstd/xz/bzip2)                 │
│   • Encryption (pkware + winzip-aes)                       │
│   • ParallelCompressor (rayon, optional)                   │
│   • ZeroCopy IO traits (Read+Seek / BufRead)               │
└────────────────────────────────────────────────────────────┘
   │                        ▲
   ▼                        │
┌────────────────────────────────────────────────────────────┐
│              Async Adapter (tokio, optional)               │
│   AsyncRead/AsyncWrite bridge → blocking engine on          │
│   a thread pool, or native async path for pure-buffer ops   │
└────────────────────────────────────────────────────────────┘
   │
   ▼
┌────────────────────────────────────────────────────────────┐
│                    C FFI Layer (crate)                     │
│   #[no_mangle] zip_* → ABI-compatible with libzip          │
│   cbindgen → zip.h                                        │
└────────────────────────────────────────────────────────────┘
```

### 5.1 Core design choices
- **Traits over concrete types:** `Source` trait mirrors the C command switch but is
  Rust-native (e.g., `fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>` plus
  `Supports`, `Seek`, `Stat`). Layering uses `Box<dyn Source>`.
- **Internal `Arc`-free, single-owner** state: an archive owns its entries and sources;
  the FFI layer adds `Mutex` only at the ABI boundary. This keeps the Rust core
  naturally `Send + Sync`-friendly.
- **Copy-on-write for `zip_source_buffer`:** wrap `Bytes` so buffer-backed sources do
  **zero-copy** reads when the consumer can consume references.
- **Cancellation & progress:** port `zip_progress` / `zip_cancel` semantics as
  cooperative poll points inside the compression loop.

### 5.2 Parallel compression (new capability)
- Split archive files into **independent compression tasks** (each file's compressed
  bytes and local header are independent; only the central directory is serialized).
- Use a **bounded work-stealing pool** (`rayon` scoped threads) with a configurable
  worker count.
- **Output determinism:** produce an ordered stream by collecting each task's result
  into a per-file slot then writing in index order, so output bytes are identical
  to serial mode (byte-for-byte) — essential for reproducibility and tests.
- Handle the **single very large file** case by falling back to serial (or intra-file
  chunked DEFLATE when the codec supports parallel blocks).

### 5.3 Zero-copy I/O (new capability)
- **Read path:** `zip_source_buffer` and file-backed reads expose `&[u8]` slices when
  the buffer is owned contiguously; avoid intermediate copy buffers by passing caller
  slices straight into codec decode.
- **Decode in place** where the codec permits, otherwise reuse a pooled staging buffer
  (`BufferPool`) to avoid per-call allocation.
- **Write path:** central directory and headers are serialized directly into the
  output sink; CRC computed via a streaming hasher that borrows slices.

### 5.4 Async streaming (new capability)
- Feature-gated `tokio` adapter. Two modes:
  - **Bridge mode:** `spawn_blocking` runs the sync engine; returns `AsyncRead`/
    `AsyncWrite` handles. Simplest, correct, good enough for I/O-bound use.
  - **Native async mode:** for pure buffer/crypto pipelines, drive the engine from an
    async `Source` using `poll_read`, avoiding thread hops. More work; ship in phase 3.
- Provide `tokio::io::AsyncReadExt`-compatible readers and `Stream` of archive entries.

---

## 6. API Surface Design

### 6.1 Idiomatic Rust API (new, primary)
```rust
// Opening
let ar: Archive = Archive::open(Path::new("a.zip"))?;      // or from reader
let ar = Archive::open_from(reader, Options::default())?;

// Reading (streaming)
for entry in ar.entries() {
    let name = entry.name()?;
    let mut r: impl Read + Seek = entry.reader()?;         // zero-copy capable
    io::copy(&mut r, &mut stdout)?;
}

// Writing (parallel)
let mut w = ArchiveWriter::new(sink, WriterOptions { parallel: true, workers: 4, .. });
w.add_file("photos/a.jpg", Path::new("a.jpg"))?;           // queued as a task
w.add_bytes("notes.txt", Bytes::from(...))?;               // zero-copy buffer source
w.finish()?;                                                // schedules + finalizes

// Async
let mut r: tokio::io::AsyncRead = ar.async_reader("big.bin")?;
```

### 6.2 C ABI compatibility layer (secondary, for drop-in)
- Export the full 139 `zip_*` symbols via a `cdylib` crate with `#[no_mangle]`.
- Represent `zip_t`, `zip_file_t`, `zip_source_t`, `zip_error_t` as opaque structs
  holding the Rust objects behind `Mutex`.
- Generate `zip.h` with `cbindgen`; keep names, flags, and error codes identical.
- Goal: pass libzip's **own** regression suite against our C ABI (see §8.4).

---

## 7. Module-by-Module Mapping (C → Rust)

| C files | Rust module | Port strategy |
|---------|-------------|---------------|
| `zip_open.c`, `zip_close.c`, `zip_discard.c`, `zip_fdopen.c` | `archive::open`, `archive::close` | **Port faithfully** (careful with flags) |
| `zip_dirent.c`, `zip_entry.c` | `directory::entry` | Port + strengthen invariants |
| `zip_add.c`, `zip_delete.c`, `zip_rename.c`, `zip_replace.c`, `zip_unchange*.c` | `archive::edit` | Port |
| `zip_source_*.c` (all) | `source::{buffer,file,window,crc,compress,layered,zip,function}` | **Re-architect** into Rust trait; this is the crown jewel |
| `zip_algorithm_*.c` | `codec::{deflate,zstd,xz,bzip2}` | Bind to crates (§4.2) |
| `zip_pkware.c`, `zip_winzip_aes.c`, `zip_crypto_*.c` | `crypto::{pkware,aes}` | Reimplement (pure Rust) |
| `zip_hash.c` | `util::hashtable` | Replace with `HashMap` |
| `zip_buffer.c` | `util::buffer` | Replace with `bytes::Bytes` |
| `zip_error*.c` | `error::ZipError` | Port preserving two-axis codes |
| `zip_extra_field*.c` | `directory::extra_field` | Port |
| `zip_name_locate.c`, `zip_utf-8.c` | `directory::names` | Port + use `encoding_rs` |
| `zip_stat*.c`, `zip_file_set_*.c` | `entry::metadata` | Port |
| `zip_random_*.c` | `rng` | Use OS RNG (`getrandom`) |
| `zip_progress.c`, `zip_register_*` | `progress` | Port as cooperative poll points |
| `zip_source_file_{stdio,win32}*` | `io::file` | Port; use `File` + platform backends |

---

## 8. Testing Strategy (Commercial-Grade)

### 8.1 Layers of testing
1. **Unit tests** — per module: dirent parse/serialize round-trips, CRC, codec,
   name encoding, error mapping.
2. **Integration tests** — end-to-end create→read→modify→re-read cycles on disk and
   in memory; all compression × encryption combinations.
3. **Property-based tests (`proptest`)** — random valid entry sets, random names,
   random sizes, random extra fields; assert round-trip invariants and that
   re-reading our own output equals the input metadata.
4. **Malformed-input robustness (`proptest` + targeted)** — feed truncated, bit-flipped,
   and adversarial archives; assert **no panic** (only `Err`) and no memory unsafety.
   Enable `-Zpanic=abort` in release-fuzz to guarantee no panics leak.
5. **Fuzzing (`libfuzzer-sys` + `cargo-fuzz`)** — continuous corpus targeting the
   central directory parser, entry reader, and codec demuxers.
6. **Differential testing vs C libzip** — the killer feature (§8.3).

### 8.2 `#[cfg(test)]` hygiene & no-panic policy
- All parse code uses checked arithmetic (`try_into`, checked offsets); reject
  overflow with `ZipError::InvalidArgument`.
- CI runs `cargo test` in **both** debug and release, plus `-Zbuild-std` with
  overflow-checks on.
- `#[deny(clippy::indexing_slicing)]` style lints in parser modules to force bounds
  checking.

### 8.3 Differential testing harness (C vs Rust)
- Build the **original C libzip** (via its CMake) and the **Rust C-ABI crate** into
  two loadable libraries exposing the same `zip_*` ABI.
- Write a **single harness** (Rust, using `libloading` or C) that loads each library,
  runs the same operations, and **compares:**
  - exit success/failure status,
  - error codes,
  - produced output bytes (byte-for-byte),
  - metadata (`zip_stat`) values.
- Run across the entire 187-case regress corpus plus generated fuzz corpus.
- This gives **high-confidence equivalence** without hand-porting each C test.

### 8.4 Reuse of libzip's own suite
- Point the Rust project's CI at `libzip/regress/*.test` (they're shell/nihtest-driven
  against the C tools). Initially run them **against the C build as a baseline**, then
  against our C ABI to prove drop-in parity.
- Port the most valuable `.test` cases into native Rust integration tests over time.

### 8.5 Sanitizers & hardening in CI
- Run Rust tests under `cargo miri` for the safe-code invariants.
- For the FFI layer, run the C regress suite compiled with `-fsanitize=address,undefined`
  linked against our Rust `cdylib` to catch ABI misuse.

---

## 9. Benchmarking Strategy

### 9.1 Tooling
- `criterion` for micro-benchmarks (stable, statistically rigorous).
- A standalone **macro-benchmark CLI** comparing the C library and the Rust library
  under identical workloads and identical compressed sizes.

### 9.2 Methodology (fair comparison)
- **Same inputs, same codec settings, same worker count, same output** on the same
  machine, multiple runs, reporting throughput (MiB/s) and memory peak (RSS).
- Control compression *ratio* by using identical codec params; measure time to a
  **given** compressed size so speed isn't bought by lower ratio.
- Report both **serial** (Rust vs C) and **parallel** (Rust 1/2/4/8 workers vs C serial).

### 9.3 Benchmarks to ship
| Benchmark | Measures |
|-----------|----------|
| Compress small files (1k–64k) | per-file overhead, scheduling |
| Compress large file (1 GiB) | single-stream throughput |
| Compress mixed corpus (many files, mixed sizes) | parallel speedup, determinism |
| Read/extract full archive | decode throughput, zero-copy benefit |
| Read random entries | random-access / seek cost |
| Modify in place (add/delete/rename) | rewrite overhead |
| Memory peak on 10k-entry archive | footprint |
| Throughput under tokio async streaming | async bridge overhead |

### 9.4 Benchmark suite layout
```
benches/
  criterion/ (micro)
  harness/   (criterion macros + C-vs-Rust runner)
  data/      (committed small corpus + scripts to generate large ones)
results/
  baseline-c.csv   # original C libzip
  rust-serial.csv
  rust-parallel.csv
```

### 9.5 Acceptance gates (define numbers in Phase 1 baselining)
- Rust **serial** ≥ C serial throughput within a tolerance (e.g., ≥90%) for codec-bound
  workloads (parity is the floor; memory safety is the real win).
- Rust **parallel** scales: ≥1.8× at 2 workers, ≥3× at 4 workers, ≥5× at 8 workers on a
  multi-file corpus, while producing **byte-identical** output to serial.

---

## 10. Security & Memory-Safety Plan
- **No `unsafe` in the core** except the FFI boundary and `zerocopy` casts (audited,
  isolated in one module, documented).
- **No panics on external input** (verified by fuzzing + `catch_unwind` fence at FFI
  boundary; better: eliminate panics in parser code).
- **Allocation caps:** bound extra-field counts, entry counts, and name lengths to
  reject zip-bombs early; document limits.
- **Codec safety:** rely on the audited upstream crates; pin exact versions and run
  `cargo audit` in CI.
- **Zip-bomb awareness:** streaming CRC + total-size tracking; provide `max_decompressed`
  guard as an option.

---

## 11. Phasing & Milestones

### Phase 0 — Scaffolding & Baselines (Week 1–2)
- [ ] Workspace skeleton, CI (GitHub Actions), linting, `cargo deny/audit`.
- [ ] Build original C libzip, record baseline `results/baseline-c.csv`.
- [ ] Set up differential harness skeleton + libloading of both libraries.

### Phase 1 — Core Engine: read path (Week 3–8)
- [ ] `error` module, `Source` trait, `buffer`/`file`/`window` sources.
- [ ] Central directory parser/writer, entry table, name/extra-field handling.
- [ ] Codec bindings + CRC.
- [ ] Serial read + extract; **pass differential read tests**.
- [ ] Establish serial-vs-C baseline numbers.

### Phase 2 — Write & edit path (Week 9–14)
- [ ] `ArchiveWriter`, entry add/replace/delete/rename, unchange semantics.
- [ ] Encryption: ZipCrypto + WinZip AES encode/decode.
- [ ] Progress/cancel hooks.
- [ ] **Pass differential write/edit tests**; byte-identical output.

### Phase 3 — New capabilities (Week 15–20)
- [ ] Parallel compression (rayon) with deterministic output.
- [ ] Zero-copy read path + buffer pooling.
- [ ] Async adapter (bridge mode first, then native poll path).
- [ ] New benchmarks: parallel scaling + zero-copy memory.

### Phase 4 — C ABI & hardening (Week 21–26)
- [ ] `#[no_mangle]` FFI layer + `cbindgen` `zip.h`.
- [ ] Run full libzip regress suite against our C ABI.
- [ ] Fuzz hardening, Miri, sanitizer CI runs.
- [ ] Docs (`mdbook`), MSRV policy, release pipeline (`cargo release`).

### Phase 5 — Commercial readiness (Week 27–30)
- [ ] Performance acceptance gates met.
- [ ] Security review, audit trail, licensing cleanup.
- [ ] SemVer 1.0, changelog, migration guide, support matrix.

**Estimated: ~30 weeks for a small (2–3 engineer) team.** Parallelizable streams:
core engine, FFI+harness, and fuzz/testing can proceed in parallel after Phase 1.

---

## 12. Risks & Mitigations
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Format-conformance bugs (subtle ZIP edge cases) | Med | High | Differential testing vs C on full corpus + fuzzing |
| Async + parallel interaction deadlocks | Med | Med | Deterministic output, bounded pools, stress tests |
| C-ABI subtle mismatches (flags, struct layout) | Med | Med | cbindgen + run C regress suite against our ABI |
| Codec perf regression vs C | Med | Med | Baseline early; allow zlib-ng native backend |
| Scope creep (no_std, native async) | Med | Med | Strict phasing; feature-gate ambitious items |
| Licensing/patent concerns | Low | High | Independent implementation; keep BSD license; legal review |
| Team ramp on complex C semantics | Med | Low | Detailed module mapping doc; pair with C source |

---

## 13. Deliverables Checklist
- [ ] `crates/zip-core` — safe Rust engine (sync, std).
- [ ] `crates/zip-async` — tokio adapter (feature-gated).
- [ ] `crates/zip-sys` — C ABI cdylib + generated `zip.h`.
- [ ] `crates/ziptools` — `zipcmp`/`zipmerge`/`ziptool` equivalents (for conformance).
- [ ] `differential/` — C-vs-Rust differential harness.
- [ ] `benches/` — criterion + macro benchmarks; `results/` baselines.
- [ ] `fuzz/` — cargo-fuzz targets.
- [ ] CI pipelines (test, fuzz, audit, bench).
- [ ] Docs: API reference, migration guide, security policy.
- [ ] `PLAN.md` + risk register + acceptance criteria.

---

## 14. Immediate Next Steps
1. Approve the workspace layout and dependency set (§4).
2. Build C libzip and produce `results/baseline-c.csv` (Phase 0).
3. Scaffold `crates/zip-core` with the `error` module and `Source` trait.
4. Stand up differential harness loading both libraries.

*This plan is iterative — numbers (throughput targets, team size) are to be pinned
once Phase 0 baselines exist.*

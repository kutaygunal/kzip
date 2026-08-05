# kzip — Phase 5 §9.3 Benchmark Report (C libzip vs Rust zip-core)

**Recorded:** 2026-08-04
**Workload machine:** local Windows, MSVC release builds, 24 logical CPUs, 31.7 GB RAM.
**Reference library:** original C libzip `libs/c/zip.dll` **v1.11.4** (MSVC release, native zlib + bzip2), loaded in-process via `libloading`.
**Rust implementation:** `zip-core` v0.1.0 (`flate2` / pure-Rust `miniz_oxide` DEFLATE), `cargo build --release` (`opt-level=3`, `lto="thin"`).

## Methodology (per PLAN §9.2)

- **Same inputs:** every C and Rust harness consumes byte-identical deterministic corpora (shared xorshift64 PRNG corpus builders in `benches/src/lib.rs`).
- **Same codec settings:** DEFLATE **level 6** for both implementations. Note: the two DEFLATE backends (native zlib vs miniz_oxide) are *both* standard DEFLATE level 6 but are different codecs, so exact compressed-size byte-identity is not expected; sizes are the same codec-settings comparison the plan requires.
- **Same machine / same process** (C loaded in-process, no IPC), multiple runs (3–5) + warmup, **median** throughput reported in MiB/s (1 MiB = 2²⁰).
- **C** is inherently a *file-writer* (`zip_open`+`zip_close`); **Rust** `compress_files`/`write_archive` produce bytes in memory. The compressed output is tiny relative to input, so this asymmetry is minor; it is noted below.
- Raw data: `results/benchmark-<workload>.csv` (one per workload, columns `impl,version,workload,run,uncompressed_bytes,seconds,mibps[,rss_bytes]`).

---

## Summary

| # | Workload | C libzip 1.11.4 | Rust zip-core 0.1.0 | Ratio (Rust/C) | Verdict |
|---|----------|-----------------|---------------------|----------------|---------|
| 1 | Compress small files (1–64 KiB, 3000 files) | 441.4 MiB/s | 639.0 MiB/s | **1.45×** | Rust **faster** |
| 2 | Compress large single file (1 GiB) | 374.8 MiB/s | 851.8 MiB/s | **2.27×** | Rust **faster** |
| 3a | Compress mixed corpus (96 files, serial) | 372.8 MiB/s | 829.7 MiB/s | **2.23×** | Rust **faster** |
| 3b | Compress mixed corpus (Rust **parallel**, 24 workers) | — | 5925.2 MiB/s | 15.9× vs C | Rust **much faster** (parallel) |
| 4 | Read / extract full archive (128 entries) | 2236.5 MiB/s | 4832.0 MiB/s | **2.16×** | Rust **faster** |
| 5 | Read random entries (2000 of 10k) | 301.2 MiB/s | 263.4 MiB/s | **0.87×** | C **slightly faster** |
| 6 | Modify in place (add/delete/rename) | 9.07 ms | 9.18 ms | ~1.01× (wall) | **parity** (see note) |
| 7 | Memory peak, 10k-entry archive | RSS Δ ≈ 0 MiB (limitation) | compress 8.9 MiB / read 0.05 MiB | — | Rust bounded; C RSS n/a |
| 8 | Async (zip-async / tokio) streaming | — | — | — | **DEFERRED** (see §8) |

---

## 1. Compress small files (1 KiB–64 KiB) — per-file overhead & scheduling

Corpus: **3000** deterministic small text files, sizes 1 KiB–64 KiB (95.4 MiB), both serial, DEFLATE 6.

| impl | median MiB/s | runs |
|------|--------------|------|
| C libzip | 441.4 | 438.1/443.5/441.4/439.6/443.4 |
| Rust zip-core | 639.0 | 635.4/638.9/621.6/650.8/647.2 |

**Ratio 1.45× — Rust faster.** On a per-file/scheduling-bound workload, `zip-core`'s lower per-entry API overhead wins: C pays a per-file `zip_source_buffer_create` + `zip_file_add` + `zip_set_file_compression` round-trip through the FFI and libzip's source layer, whereas Rust iterates `compress_files` over in-memory `ArchiveFile`s directly. Consistent with the memory-workload note, C's per-file overhead is its weakest point.

## 2. Compress large single file (1 GiB) — single-stream throughput

Corpus: one **1024 MiB** highly-compressible payload, DEFLATE 6. A single file always takes the serial path in both implementations (no parallel split), so this isolates raw codec throughput.

| impl | median MiB/s | runs |
|------|--------------|------|
| C libzip | 374.8 | 377.6/374.8/373.8 |
| Rust zip-core | 851.8 | 850.9/851.8/860.5 |

**Ratio 2.27× — Rust faster.** `miniz_oxide` is markedly faster than native zlib on highly repetitive input. (On *mixed/random* input the opposite holds — see the cross-check below — so the winner is content-dependent.)

> Cross-check against the committed phase-5 harness on the original mixed corpus (`c_serial`/`rust_serial`): C **314.6** vs Rust **259.7** MiB/s → Rust = **82.6%** of C, reproducing the phase-5 "serial gate not met" finding. The §1/§2/§3a Rust advantage here is specific to compressible-text corpora; on the mixed corpus (with incompressible random data) native zlib wins. This confirms the gap is **codec-backend-dependent**, not an architecture difference.

## 3. Compress mixed corpus (many files) — parallel speedup + determinism

Corpus: **96** files, 128 KiB–2 MiB (73.8 MiB), all below zip-core's 8 MiB large-file threshold (so the whole batch parallelizes). C is single-threaded by design; Rust runs on the default rayon pool (24 workers).

| impl / mode | median MiB/s |
|-------------|--------------|
| C serial | 372.8 |
| Rust **serial** | 829.7 |
| Rust **parallel** (24 workers) | 5925.2 |

- **Rust serial vs C serial:** 2.23× faster (text corpus).
- **Rust parallel speedup:** 5925.2 / 829.7 = **7.14×** over Rust serial; **15.9×** vs C serial.
- **Determinism:** the harness asserted Rust parallel output is **byte-identical** to Rust serial (CRC + bytes) — **PASS**, consistent with the committed `parallel` benchmark's invariant.

## 4. Read / extract full archive — decode throughput

Archive: **128** deflated entries (97.8 MiB uncompressed) written to disk; both sides open the same file and read every entry to EOF. C via `zip_open`+`zip_fopen_index`+`zip_fread`; Rust via `Archive::open(File)`+`EntryReader`.

| impl | median MiB/s | runs |
|------|--------------|------|
| C libzip | 2236.5 | 2263.6/2248.3/2236.5/2165.9/2219.6 |
| Rust zip-core | 4832.0 | 4816.4/4832.0/4662.5/4851.8/4832.5 |

**Ratio 2.16× — Rust faster.** Both stream-decode from disk; Rust's `EntryReader`/flate2 inflate is faster than libzip's layered source pipeline for these small deflate blocks.

## 5. Read random entries — random-access / seek cost

Archive: **10 000** entries; deterministically read **2000** random entries to EOF from the same on-disk file (4.4 MiB read). C `zip_fopen_index` (source seek); Rust `Archive::open_entry` (per-entry `Source::duplicate()` = cheap file-handle clone + seek).

| impl | median MiB/s |
|------|--------------|
| C libzip | 301.2 |
| Rust zip-core | 263.4 |

**Ratio 0.87× — C slightly faster (Rust ~87% of C).** On a seek/latency-bound workload with tiny entries, C is a bit ahead; the difference is modest and mostly per-entry open+seek overhead. (An in-memory `Cursor` source is *not* used here because `Source::duplicate()` clones the whole backing buffer per `open_entry`, which would conflate seek cost with a large memcpy — an architectural note: buffer-backed random access in zip-core currently copies the buffer per entry.)

## 6. Modify in place (add/delete/rename) — **rewrite model**

- **C libzip** supports true in-place modification (`zip_open` + `zip_delete` + `zip_rename` + `zip_close`): it reuses already-compressed member data and only rewrites the central directory / temp-file-rename.
- **Rust `zip-core` has no in-place edit API.** Per the plan, the Rust side is built as a **rewrite model** (`read` source corpus + `write_archive`, recompressing every member). This is an apples-to-oranges comparison: the Rust rewrite does strictly more work.
- Corpus 48.4 MiB (64 files); 3 renames + 5 deletes.

| impl | median wall time | effective MiB/s (of 48.4 MiB) |
|------|------------------|-------------------------------|
| C in-place | **9.07 ms** | 5336.2 |
| Rust rewrite (recompress all) | **9.18 ms** | 5271.0 |

**Verdict: near parity in wall time**, but the two do different work. C in-place skips recompression (only directory rewrite) yet still lands at ~9 ms because it reads the original, rewrites, and temp-renames the file. Rust recompresses all 48 MiB in ~9 ms thanks to parallel DEFLATE. Neither is meaningfully faster; the honest conclusion is that **zip-core's rewrite model reaches C's in-place speed only because its parallel compression is fast** — a dedicated in-place path (reuse compressed data) is not implemented and would be expected to be much faster still.

## 7. Memory peak on a 10k-entry archive (22.0 MiB)

- **Rust** (counting global allocator, high-water-mark live bytes): compress `write_archive` peak **8.9 MiB**; read-all (streaming decode from file) peak **0.05 MiB**. Decode memory is bounded and tiny (entries decoded one-at-a-time, small inflate state).
- **C** (approximate RSS delta via `GetProcessMemoryInfo` working set): compress Δ and read Δ read **≈ 0 MiB** in most runs.

**Caveat / limitation:** C's RSS-delta method cannot reliably capture the transient peak because the process allocator reuses freed memory (deltas were 0 on most runs). This is a methodological limitation of in-process RSS sampling; an out-of-process / instrumented zlib allocator would be required for an exact C peak. Reported numbers therefore are not directly comparable (Rust = allocator-count peak, C = working-set delta, which underestimates). Rust's own footprint is small and bounded, which is the defensible result from this workload.

## 8. Async (zip-async / tokio) streaming — **DEFERRED**

Deferred, with reason: it adds significant scope (a separate async read/write path through the `zip-async` crate and a tokio runtime under benchmark load) and the C libzip side has no direct async equivalent to compare fairly (it is synchronous). The synchronous read/decode path (workloads 4/5) already covers the underlying codec throughput; async adds only bridge/task overhead that is out of scope for a fair C-vs-Rust comparison here. Recommend a follow-up `zip-async`-only throughput measurement against its own tokio baseline.

---

## Fairness & limitation notes

1. **C writes a file, Rust produces memory bytes** in the compress workloads (1–3). The compressed output is small relative to input, so the asymmetry is minor; it makes C's numbers include a small `zip_close` write step.
2. **Codec-backend dependence.** Rust (miniz_oxide) wins on compressible/repetitive text; C (native zlib) wins on mixed/random data. Results must be read in terms of the specific corpus. The committed phase-5 gate (Rust ≥90% of C on the *mixed* corpus) remains unmet (≈83%) because that corpus is mixed.
3. **Read paths.** Full-archive read uses a file source for both (fair); the in-memory zero-copy path (`decode_slice_into`, `BufferPool`) is covered by the committed `zerocopy` benchmark and, for buffer-backed sources, `Source::duplicate()` clones the backing buffer per `open_entry` (see workload 5 note).
4. **Modify** is a rewrite-model vs in-place comparison by necessity (Rust has no in-place API).

## How to reproduce

```
cargo build --release -p libzip-benches
./target/release/bench_small.exe      # -> results/benchmark-small.csv
./target/release/bench_large.exe      # -> results/benchmark-large.csv
./target/release/bench_mixed.exe      # -> results/benchmark-mixed.csv
./target/release/bench_read.exe       # -> results/benchmark-read.csv
./target/release/bench_random.exe     # -> results/benchmark-random.csv
./target/release/bench_modify.exe     # -> results/benchmark-modify.csv
./target/release/bench_memory.exe     # -> results/benchmark-memory.csv
```
C DLL path overridable with `ZIP_DLL`.

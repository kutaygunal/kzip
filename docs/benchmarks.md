# Benchmarks

Criterion benchmarks live in `benches/` (workspace member `libzip-benches`).

```sh
cargo bench -p libzip-benches
```

## Headline report & charts

The authoritative C-vs-Rust numbers (C libzip **1.11.4**, MSVC release, same machine,
DEFLATE 6, median of multiple runs) live in
**[`results/benchmark-report.md`](../results/benchmark-report.md)**. The full workload
CSVs are `results/benchmark-<workload>.csv`.

Headline ratios on the benchmark machine: **16.6×** parallel compression, **2.2×**
full-archive read, **8.2×** true in-place modify (all vs C). Charts rendered from the
raw data are checked into `docs/benchmarks/`:

| Chart | Shows |
|-------|-------|
| [benchmark-throughput.png](benchmarks/benchmark-throughput.png) | C libzip vs kzip throughput (compress / read / modify) |
| [benchmark-memory.png](benchmarks/benchmark-memory.png) | memory usage across read workloads |
| [benchmark-modify.png](benchmarks/benchmark-modify.png) | in-place modify timings |
| [benchmark-parallel.png](benchmarks/benchmark-parallel.png) | parallel-compression scaling |
| [zip-tools-compress.png](benchmarks/zip-tools-compress.png) / [zip-tools-extract.png](benchmarks/zip-tools-extract.png) / [zip-tools-ratio.png](benchmarks/zip-tools-ratio.png) | `ziptools` compression/extraction/ratio comparisons |

## C-vs-Rust serial comparison (Phase 5)

Two harness binaries compress the *same* deterministic mixed corpus and report
serial throughput (MiB/s) as CSV, for the Rust-serial-vs-C gate (§9.5, ≥90% of C):

```sh
bash scripts/bench-serial.sh            # runs both, prints the ratio
cargo run -p libzip-benches --bin c_serial     # -> results/c-serial.csv
cargo run -p libzip-benches --bin rust_serial  # -> results/rust-serial.csv
```

See `results/phase5-benchmarks.md` for the latest measured numbers and an honest
gate-by-gate assessment.

## `parallel`

- `compress_serial` — baseline.
- `compress_parallel/{1,2,4,8}` — scaling across worker counts on a 64-file
  corpus.
- Asserts the determinism invariant: parallel output is byte-identical to
  serial, and a single very large file falls back to serial.

## `zerocopy`

Uses a counting global allocator to report bytes allocated:

- `decode_zero_copy_slice` — decode directly from a borrowed `&[u8]` slice.
- `decode_copied_input` — a path that first copies the compressed input.
- `decode_with_buffer_pool` — reuse a pre-sized `BufferPool` buffer across
  iterations.

## Methodology

Fair comparison (C vs Rust) requires identical inputs, codec settings, worker
counts, and outputs; see `PLAN.md` §9 for the full methodology and acceptance
gates.

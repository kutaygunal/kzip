# kzip

A from-scratch, memory-safe **Rust port of [libzip](https://libzip.org/)**: read,
create, and modify ZIP archives with a familiar `zip_*`-style API, plus a drop-in
`zip-sys` C ABI for existing consumers.

## What this is

- **`crates/zip-core`** — the safe Rust core engine (read/write/central-directory,
  DEFLATE via `flate2`, parallel compression with rayon, zero-copy decode buffers).
- **`crates/zip-async`** — non-blocking archive I/O over tokio.
- **`crates/zip-sys`** — a `zip_*` C ABI shim for drop-in compatibility with libzip.
- **`crates/ziptools`** — CLI tooling built on the core.
- **`libs/c/`** — the original C libzip reference library used for differential
  testing and benchmarking.

The project mirrors libzip's design while prioritizing **correctness, memory
safety, deterministic parallel output**, and **byte-for-byte compatibility** with
the C implementation.

## Status

Core read/write/compress paths are implemented and verified **byte-for-byte
equivalent** to C libzip (v1.11.4). Parallel compression is deterministic and
outperforms the C single-threaded path on multi-file workloads; serial codec
parity on mixed data is codec-backend-dependent (see the benchmark report).

## Benchmarks

Median throughput, **C libzip 1.11.4** vs **kzip (Rust)**, on identical
deterministic corpora (DEFLATE level 6, same machine, 24 logical CPUs).
*Higher is better.*

![Throughput: C libzip vs kzip](docs/benchmarks/benchmark-throughput.png)

![Parallel compression speedup](docs/benchmarks/benchmark-parallel.png)

![Modify in-place latency](docs/benchmarks/benchmark-modify.png)

![Memory footprint](docs/benchmarks/benchmark-memory.png)

| Workload | C libzip 1.11.4 | kzip (Rust) | Ratio | Verdict |
|----------|-----------------|-------------|-------|---------|
| Compress small (1–64 KiB) | 441.7 MiB/s | 650.9 MiB/s | **1.47×** | Rust faster |
| Compress large (1 GiB) | 369.6 MiB/s | 835.8 MiB/s | **2.26×** | Rust faster |
| Compress mixed (serial) | 372.9 MiB/s | 855.3 MiB/s | **2.29×** | Rust faster |
| Compress mixed (**parallel**, 24) | — | 6203.4 MiB/s | **16.6×** vs C | Rust much faster |
| Read full archive | 2237.5 MiB/s | 4991.2 MiB/s | **2.23×** | Rust faster |
| Read random entries | 319.1 MiB/s | 579.5 MiB/s | **1.82×** | Rust faster |
| Modify in place | 1.91 ms | 0.23 ms (in-place) / 9.3 ms (rewrite) | **8.2×** | Rust in-place **faster** |

> Full methodology, per-workload detail, and raw data:
> [results/benchmark-report.md](results/benchmark-report.md).

### kzip vs other zip / compression tools

Median timing on a **79.2 MiB** corpus (mix of compressible text + incompressible
random data), 5 iterations, same machine. **ZIP-format tools** (kzip, 7-Zip,
Info-ZIP) are directly comparable; **Zstandard** and **LZ4** use their own
single-stream containers and are shown only as general-compression context.
*Faster is better for time; lower ratio is better.*

![Zip-tools compress throughput](docs/benchmarks/zip-tools-compress.png)

![Zip-tools extract throughput](docs/benchmarks/zip-tools-extract.png)

![Zip-tools compression ratio](docs/benchmarks/zip-tools-ratio.png)

| tool | format | compress | extract | ratio | vs kzip (compress) |
|------|--------|---------:|--------:|------:|-------------------:|
| **kzip** (Rust) | ZIP | **44 ms** (1797 MiB/s) | 15 ms | 0.234 | 1.00× |
| 7-Zip 26.02 | ZIP | 140 ms (564 MiB/s) | **6 ms** | 0.234 | 0.31× |
| Info-ZIP zip 3.0 | ZIP | 595 ms (133 MiB/s) | 387 ms | 0.234 | 0.07× |
| Zstandard 1.5.7* | ZSTD | 28 ms (2879 MiB/s) | 16 ms | 0.231 | 1.60× |
| LZ4 1.10.0* | LZ4 | 43 ms (1821 MiB/s) | 22 ms | 0.234 | 1.01× |

\* non-ZIP single-stream containers — context only.

In the ZIP format kzip compresses **~3× faster than 7-Zip** and far faster than
Info-ZIP at an identical ratio; 7-Zip wins on **extract** speed. Full analysis and
caveats: [results/zip-tools-benchmark.md](results/zip-tools-benchmark.md).

## Reports

- 📊 **Benchmark report — C libzip vs Rust zip-core (HTML, self-contained):**
  [results/benchmark-report.html](results/benchmark-report.html)
  *(Markdown source: [results/benchmark-report.md](results/benchmark-report.md))*
- ✅ **Equivalence verification report — C libzip vs Rust zip-core:**
  [results/verification-report.md](results/verification-report.md)
- 📦 **Third-party zip-tools benchmark — kzip vs 7-Zip / Info-ZIP / zstd / lz4:**
  [results/zip-tools-benchmark.md](results/zip-tools-benchmark.md)

The benchmark report covers the §9.3 workload matrix (small / large / mixed
compression, full-archive and random reads, modify-in-place, memory peak),
reports median throughput and ratios with a verdict per workload, and states
where a workload was deferred.

## Layout

```
crates/zip-core     # safe core engine (compress, read, parallel, zero-copy)
crates/zip-async    # tokio-based async I/O
crates/zip-sys      # zip_* C ABI drop-in shim
crates/ziptools     # CLI tools
benches             # Criterion + C-vs-Rust benchmark harnesses
differential        # C-vs-Rust equivalence harness
libs/c              # original C libzip reference library
results             # benchmark CSVs and reports
docs                # design and phase documentation
```

## Build & test

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## License

BSD-3-Clause (matching libzip). See [LICENSE](LICENSE).

# Benchmarks

Criterion benchmarks live in `benches/` (workspace member `libzip-benches`).

```sh
cargo bench -p libzip-benches
```

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

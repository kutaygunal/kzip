# Benchmark suite

`differential/src/bin/benchmark.rs` is a reproducible comparison runner for
the original C libzip DLL and the Rust `zip-sys` DLL. It loads both libraries
dynamically and calls the same C ABI symbols.

Each case has two warmups and seven measured samples by default. The JSON
records every sample, median, p95, throughput, checksum, workload size, host,
compiler, and library version. Writes time `zip_open` through `zip_close`;
reads time `zip_open`, all entry reads, and `zip_close`.

The read cases use the same canonical C-written archive for both engines. The
write cases validate the resulting archive through the producing engine before
recording the result. The runner does not claim that a single machine's timing
represents all platforms.

## Bottlenecks addressed

The first snapshot exposed a pathological Rust read case: the FFI layer decoded
every entry eagerly into a new allocation and then copied it again through
`zip_fread`. The many-small Store read fell to about 7 MiB/s. The optimized path
uses read-only mmap, shares immutable archive bytes with Store readers, and
routes DEFLATE through `EntryReader` instead of the old eager `read_entry` plus
FFI copy; it also removes the redundant buffer copy when adding plain in-memory
sources. The current snapshot beats C in 19 of 20
rows. Mixed DEFLATE write remains the final optimization target and is close
enough to require repeated runs before claiming a stable win.

```powershell
cargo build --release -p differential -p zip-sys
cargo run --release -p differential --bin benchmark -- `
  libs/c/zip.dll target/release/zip.dll `
  results/benchmark-$(Get-Date -Format yyyy-MM-dd).json `
  --samples 7 --warmups 2
python benchmarks/render.py `
  results/benchmark-$(Get-Date -Format yyyy-MM-dd).json `
  docs/benchmarks/benchmark.svg docs/benchmarks/benchmark-animation.svg
```

The current workload set is intentionally varied:

- `tiny-mixed`: 64 files × 4 KiB;
- `many-small`: 1,024 files × 4 KiB;
- `text-8m`: 8 × 1 MiB compressible files;
- `mixed-8m`: alternating compressible and pseudo-random 1 MiB files;
- `single-16m`: one 16 MiB compressible file.

Each workload is exercised with Store and DEFLATE for both write and read.
Bzip2 is not included in the timing chart because the current C baseline can
write Bzip2 while the Rust layer only decodes it; that mismatch is documented
in the second gap analysis and should be resolved before adding a fair Bzip2
comparison.

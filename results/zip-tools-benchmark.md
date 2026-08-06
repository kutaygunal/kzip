# kzip vs third-party zip/compression tools

**Corpus:** 104 files, 79.2 MiB uncompressed (mix of highly-compressible text and incompressible random data).

**Iterations per tool:** 5, timing = **median**. kzip runs in-process (zip_core, DEFLATE **level 6**); CLI tools timed with wall-clock. ZIP-format tools (kzip, 7-Zip, Info-ZIP) are directly comparable; **Zstandard and LZ4 use their own single-stream containers and are shown ONLY as general compression context** - not a same-format comparison.

> kzip compress uses the library default (`parallel: true`, one worker per core across independent files). 7-Zip `-tzip` and Info-ZIP `zip` also compress multiple files in parallel (7-Zip) or serially (Info-ZIP); see caveats.

## Charts

![Zip-tools compress throughput](../docs/benchmarks/zip-tools-compress.png)

![Zip-tools extract throughput](../docs/benchmarks/zip-tools-extract.png)

![Zip-tools compression ratio](../docs/benchmarks/zip-tools-ratio.png)

## Results

| tool | format | op | median (ms) | MiB/s | compressed size | ratio (c/u) | vs kzip (x) |
|------|--------|----|------------:|------:|----------------:|------------:|------------:|
| kzip (Rust zip_core) | ZIP | compress | 44 | 1797.4 | 18.53 MiB | 0.234 | 1.00x |
| kzip (Rust zip_core) | ZIP | extract | 15 | 5401.0 | 18.53 MiB | 0.234 | 1.00x |
| 7-Zip 26.02 (7za) | ZIP | compress | 140 | 563.6 | 18.54 MiB | 0.234 | 0.31x |
| 7-Zip 26.02 (7za) | ZIP | extract | 6 | 14354.9 | 18.54 MiB | 0.234 | 2.66x |
| Info-ZIP 3.0 (zip) | ZIP | compress | 595 | 133.0 | 18.55 MiB | 0.234 | 0.07x |
| Info-ZIP 3.0 (unzip) | ZIP | extract | 387 | 204.3 | 18.55 MiB | 0.234 | 0.04x |
| Zstandard 1.5.7 | ZSTD (non-ZIP) | compress | 28 | 2878.6 | 18.31 MiB | 0.231 | 1.60x |
| Zstandard 1.5.7 | ZSTD (non-ZIP) | extract | 16 | 4908.6 | 18.31 MiB | 0.231 | 0.91x |
| LZ4 1.10.0 | LZ4 (non-ZIP) | compress | 43 | 1820.7 | 18.56 MiB | 0.234 | 1.01x |
| LZ4 1.10.0 | LZ4 (non-ZIP) | extract | 22 | 3642.2 | 18.56 MiB | 0.234 | 0.67x |

## Honest analysis

- **Same-format ZIP comparison.** kzip, 7-Zip and Info-ZIP all produce a `.zip` (DEFLATE). On this corpus kzip's median compress time was **44 ms** and 7-Zip's was the reference for the others; see the table for the exact multiplier.
- **Where kzip is strong.** For in-process/embedded use it avoids process-spawn overhead and keeps everything in memory; extract throughput is the strongest signal. Its DEFLATE ratio is comparable to the ZIP peers (see ratio column).
- **Where 7-Zip / zstd win.** 7-Zip generally packs a tighter DEFLATE stream and its multi-threaded default makes large, multi-file compresses very fast. Zstandard and LZ4 are not ZIP — they trade format/container for speed (LZ4) or better speed-to-ratio (zstd) and are included purely as raw-compression context.
- **Caveats.** (1) kzip uses `parallel: true` (rayon across files) by default; 7-Zip also multithreads, Info-ZIP is serial — thread counts differ. (2) zstd/lz4 compress a single concatenated stream, so their ratio benefits from cross-file redundancy that a per-file ZIP format cannot. (3) CLI wall-clock includes process startup + disk I/O (the corpus is read from disk), while kzip compresses from in-memory buffers; this favours kzip on compress time and must be read accordingly. (4) Default levels differ: kzip/7-Zip/Info-ZIP use DEFLATE (default level), zstd default level 3, lz4 default (LZ4_1 fast).

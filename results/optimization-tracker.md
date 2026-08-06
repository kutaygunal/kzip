# kzip read_random Optimization Tracker

Goal: close the read_random gap (Rust 0.79x vs C) without regressing other benchmarks.
Source analysis: read-random-analyzer (P0-P4 recommendations).

| Opt | Description | Status | Engineer | Test | Committed |
|-----|-------------|--------|----------|------|-----------|
| P0 | Lightweight local-header skip + remove redundant seek/tell in open_dirent | DONE | senior-engineer | zip-core tests (97 pass) | e6cbed9 |
| P1 | Avoid per-entry DuplicateHandle (shared Arc<Mutex<File>> handle) | DONE | senior-engineer | zip-core tests (98 pass) | 2caa995 |
| P2 | Reduce per-entry decode allocations (smaller/pooled buffer) | DONE | senior-engineer | zip-core tests (98 pass) | (not committed) |
| P3 | Cache data offsets in Dirent | PENDING | | | |
| P4 | mmap-backed source (zero-copy random access) | PENDING | | | |

Baseline read_random: C=323.3 MiB/s, Rust=255.9 MiB/s (0.79x).

P0 result (this run): Rust read_random = 287.9 MiB/s (median, up from 255.9);
read_full = 4828 MiB/s (no regression vs ~4827-4852 baseline).

P1 result (this run): Rust read_random = ~308 MiB/s (median, up from 287.9;
C ~326); read_full = ~4890 MiB/s (no regression vs 4828 baseline).

P1 approach: `Archive` now holds a shared `Arc<Mutex<SharedFileState>>` file
handle (created once at open via a single `try_clone`). `open_dirent`/
`read_compressed_entry` open per-entry readers via `SharedFile::at_offset`,
which shares the handle (no per-entry `DuplicateHandle`). Each reader tracks
its own logical position; every read locks the shared mutex and seeks only if
the shared OS pointer is not already at the reader's position (correct for
concurrent readers, no redundant seek for sequential reads). `last_pos` is
initialized to a sentinel so the first read always seeks (a duplicated handle
shares the OS pointer with the original, which `read_central_dir` has moved).

P2 result (this run): Rust read_random = ~312 MiB/s (median, up from ~308;
C ~291-324, Rust now at/above C); read_full = ~4907 MiB/s (no regression vs
~4890 baseline).

P2 approach: `Decoder::new` (and `decode_slice_into`) now size the flate2
`BufReader` to the entry's compressed size, capped at the 8 KiB default
(`(comp_size as usize).clamp(1, 8192)`), instead of always allocating a full
8 KiB buffer per entry. Tiny entries (the read_random workload, ~512 B..=4 KiB)
allocate only what they need; large entries keep the 8 KiB buffer, so read_full
throughput is unchanged. Correctness is unaffected (a smaller buffer merely
means more underlying reads); CRC/size verification is untouched.

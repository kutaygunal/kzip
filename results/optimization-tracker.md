# kzip read_random Optimization Tracker

Goal: close the read_random gap (Rust 0.79x vs C) without regressing other benchmarks.
Source analysis: read-random-analyzer (P0-P4 recommendations).

| Opt | Description | Status | Engineer | Test | Committed |
|-----|-------------|--------|----------|------|-----------|
| P0 | Lightweight local-header skip + remove redundant seek/tell in open_dirent | DONE | senior-engineer | zip-core tests (97 pass) | |
| P1 | Avoid per-entry DuplicateHandle (shared Arc<Mutex<File>> handle) | PENDING | | | |
| P2 | Reduce per-entry decode allocations (smaller/pooled buffer) | PENDING | | | |
| P3 | Cache data offsets in Dirent | PENDING | | | |
| P4 | mmap-backed source (zero-copy random access) | PENDING | | | |

Baseline read_random: C=323.3 MiB/s, Rust=255.9 MiB/s (0.79x).

P0 result (this run): Rust read_random = 287.9 MiB/s (median, up from 255.9);
read_full = 4828 MiB/s (no regression vs ~4827-4852 baseline).

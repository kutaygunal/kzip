# Modify in-place optimization tracker

Goal: make kzip's **Modify in place** benchmark clearly faster than C libzip, without
regressing other benchmarks.

Baseline (re-measured): C in-place 9.55 ms vs Rust rewrite 9.69 ms (48.4 MiB, 64-file
corpus). Rust already ~parity despite doing strictly more work (recompresses every
member). Headroom: give Rust a TRUE in-place path that reuses compressed member data and
rewrites only the central directory + EOCD.

Analyzer: modify-analyzer (MODIFY_ANALYSIS_DONE).

| # | Phase | Status | Engineer | Test | Commit |
|---|-------|--------|----------|------|--------|
| M0 | Harness & measurement baseline | DONE (folded into M3) | — | — | — |
| M1 | CD serializer for Dirent + expose cdir_offset | DONE | engineer-m1 | testing-m1 (M1_TEST_PASS) | 3b9326b |
| M2 | Byte-array true in-place modify | DONE | engineer-m2 | testing-m2 (M2_TEST_PASS) | 1901852 |
| M3 | File-based in-place write + wire benchmark | DONE | engineer-m3 | testing-m3 (M3_TEST_PASS) | 4999178 |
| M4 | Hardening: ZIP64, overflow, regression guard | DONE | engineer-m4 | testing-m4 (M4_TEST_PASS) | e183f5c |
| M5 | (optional) local-header parity | — | | | |
| M6 | (optional) non-seekable fallback + polish | — | | | |

Expected end state (M1+M2+M3): modify_inplace median ≪ C. Achieved: Rust in-place ~0.25 ms vs C ~2 ms (≈8× faster) and ~40× faster than the rewrite, measured with a fair fsync-after-restore methodology applied equally to both sides.

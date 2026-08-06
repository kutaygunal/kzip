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
| M1 | CD serializer for Dirent + expose cdir_offset | DONE | senior-engineer | results/modify-tests (M1 round-trip) | (this commit) |
| M2 | Byte-array true in-place modify | PENDING | | | |
| M3 | File-based in-place write + wire benchmark | PENDING | | | |
| M4 | Hardening: ZIP64, overflow, regression guard | PENDING | | | |
| M5 | (optional) local-header parity | — | | | |
| M6 | (optional) non-seekable fallback + polish | — | | | |

Expected end state (M1+M2+M3): modify_inplace median ≪ C (30–100× faster), because only
~5 KiB of central directory is rewritten instead of recompressing 48 MiB.

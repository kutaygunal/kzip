#!/usr/bin/env bash
# Verification driver: generates the edge-case corpus, runs the extended
# read-path differential harness against BOTH the original C libzip and the Rust
# zip-sys cdylib, diffs the JSON, and runs the cross-read (write-path) check.
#
# Outputs (all NEW files; never overwrites c-baseline.json or the phase-5
# differential outputs):
#   results/verify-read.json       — C libzip read result
#   results/verify-read-rust.json  — Rust cdylib read result
#   results/verify-read.diff       — diff of the two
#   results/verify-crossread.json  — cross-read / write-path result
#   results/verification-report.md — full report
#
# Usage: bash run-verify.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
LIB_C="$ROOT/libs/c/zip.dll"
LIB_RUST="$ROOT/target/release/zip.dll"
CORPUS="$ROOT/data/corpus-verify"
OUT="$ROOT/results"

mkdir -p "$OUT"

echo "== Building differential + cdylib (differential only; benches left untouched) =="
cargo build --release --package differential --package zip-sys

if [ ! -f "$LIB_RUST" ]; then
  echo "!! Rust cdylib missing at $LIB_RUST"; exit 1
fi

echo "== Generating deterministic corpus with C libzip (write API) =="
"$ROOT/target/release/gen_corpus.exe" "$LIB_C" "$CORPUS" || { echo "gen_corpus failed"; exit 1; }
echo "corpus archives: $(find "$CORPUS" -name '*.zip' | wc -l)"

echo "== Running extended read-path harness against C libzip =="
"$ROOT/target/release/verify_read.exe" "$LIB_C" "$CORPUS" > "$OUT/verify-read.json" 2> "$OUT/verify-read.err" \
  || { echo "C verify_read failed:"; cat "$OUT/verify-read.err"; exit 1; }

echo "== Running extended read-path harness against Rust cdylib =="
"$ROOT/target/release/verify_read.exe" "$LIB_RUST" "$CORPUS" > "$OUT/verify-read-rust.json" 2> "$OUT/verify-read-rust.err" \
  || { echo "Rust verify_read failed:"; cat "$OUT/verify-read-rust.err"; exit 1; }

echo "== Diffing C vs Rust read-path results =="
if diff -u "$OUT/verify-read.json" "$OUT/verify-read-rust.json" > "$OUT/verify-read.diff"; then
  echo "READ-PATH: PASS (byte-identical JSON across all archives)"
  READ_RESULT="PASS"
else
  echo "READ-PATH: DIFFERENCES FOUND -> $OUT/verify-read.diff"
  READ_RESULT="DIFF"
fi

echo "== Running cross-read / write-path check =="
"$ROOT/target/release/cross_read.exe" "$LIB_C" "$CORPUS" > "$OUT/verify-crossread.json" 2> "$OUT/verify-crossread.err" \
  || { echo "cross_read failed:"; cat "$OUT/verify-crossread.err"; exit 1; }

echo
echo "================ SUMMARY ================"
echo "Read-path equivalence: $READ_RESULT"
grep "cross_read" "$OUT/verify-crossread.err" 2>/dev/null || true
echo "Full report: $OUT/verification-report.md"

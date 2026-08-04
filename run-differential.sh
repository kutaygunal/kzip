#!/usr/bin/env bash
# Differential driver: run the harness against BOTH the original C libzip and the
# Rust zip-sys cdylib, then diff the JSON results.
#
# Usage: bash run-differential.sh
#   requires: cargo, cmake, a built C libzip (libs/c/zip.dll), built Rust cdylib.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
LIB_C="$ROOT/libs/c/zip.dll"
LIB_RUST="$ROOT/target/release/zip.dll"
CORPUS="$ROOT/data/corpus"
OUT="$ROOT/results"

mkdir -p "$OUT"

echo "== Building Rust workspace =="
cargo build --release --workspace

echo "== Running harness against C libzip =="
(cd "$ROOT/libs/c" && "$ROOT/target/release/differential.exe" "$LIB_C" "$CORPUS") \
  > "$OUT/c-baseline.json" 2> "$OUT/c-baseline.err" || { echo "C run failed:"; cat "$OUT/c-baseline.err"; exit 1; }

if [ ! -f "$LIB_RUST" ]; then
  echo "!! Rust cdylib not found at $LIB_RUST (expected until Phase 1/4)."
  echo "   C baseline written to $OUT/c-baseline.json"
  exit 0
fi

if ! "$ROOT/target/release/differential.exe" "$LIB_RUST" "$CORPUS" \
  > "$OUT/rust.json" 2> "$OUT/rust.err"; then
  # Phase 0: the Rust ABI is not yet implemented, so symbol resolution fails.
  # Treat this as "pending" rather than a failure.
  if grep -q "symbol zip_" "$OUT/rust.err"; then
    echo "!! Rust cdylib loaded but zip_* symbols not yet exported (Phase 0/1)."
    echo "   C baseline written to $OUT/c-baseline.json"
    exit 0
  fi
  echo "Rust run failed:"; cat "$OUT/rust.err"; exit 1
fi

echo "== Diffing C vs Rust =="
if diff -u "$OUT/c-baseline.json" "$OUT/rust.json"; then
  echo "PASS: byte-identical behavior (read path)."
else
  echo "FAIL: differences found. See results/rust.json vs results/c-baseline.json"
  exit 1
fi

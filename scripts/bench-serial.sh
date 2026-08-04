#!/usr/bin/env bash
# C-vs-Rust serial throughput comparison (Phase 5, gate A.1).
#
# Runs the same DEFLATE codec-bound corpus through C libzip (via libloading) and
# Rust zip-core, writes results/c-serial.csv + results/rust-serial.csv, and prints
# the Rust/C throughput ratio against the 90% acceptance gate.
#
# Usage: bash scripts/bench-serial.sh [path-to-zip.dll]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DLL="${1:-$ROOT/libs/c/zip.dll}"
OUT="$ROOT/results"
mkdir -p "$OUT"

echo "== Building benchmark harnesses =="
cargo build --release -p libzip-benches

echo "== C libzip serial =="
(cd "$ROOT" && "$ROOT/target/release/c_serial.exe" "$DLL") >/dev/null

echo "== Rust zip-core serial =="
(cd "$ROOT" && "$ROOT/target/release/rust_serial.exe") >/dev/null

c=$(awk -F, 'NR>1{print $6}' "$OUT/c-serial.csv" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
r=$(awk -F, 'NR>1{print $6}' "$OUT/rust-serial.csv" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
ratio=$(awk -v r="$r" -v c="$c" 'BEGIN{printf "%.1f", r/c*100}')
echo "C median:     ${c} MiB/s"
echo "Rust median:  ${r} MiB/s"
echo "Ratio:        ${ratio}%  (gate: >=90%)"

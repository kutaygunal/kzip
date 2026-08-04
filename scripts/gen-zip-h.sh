#!/usr/bin/env bash
# Regenerate crates/zip-sys/include/zip.h with cbindgen.
#
# cbindgen is NOT required for normal builds (the committed header is kept in
# sync by hand). Use this script only when you have cbindgen installed:
#
#   cargo install cbindgen
#   bash scripts/gen-zip-h.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v cbindgen >/dev/null 2>&1; then
  echo "cbindgen not installed. Install with: cargo install cbindgen"
  echo "Using the committed, hand-maintained crates/zip-sys/include/zip.h."
  exit 0
fi

(cd "$ROOT/crates/zip-sys" && cargo build --release)

cbindgen --crate zip --output "$ROOT/crates/zip-sys/include/zip.h"
echo "Regenerated crates/zip-sys/include/zip.h"

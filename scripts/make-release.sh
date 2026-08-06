#!/usr/bin/env bash
# Build, package, and (optionally) code-sign the kzip Windows release.
#
# Usage:
#   bash scripts/make-release.sh                 # build + package (unsigned)
#   bash scripts/make-release.sh <pfx> [password]  # build + package + sign
#
# Environment:
#   KZIP_CERT_PASSWORD  certificate password (if [password] arg omitted)
#   KZIP_TIMESTAMP_URL  RFC3161 timestamp server (default: DigiCert)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -E '^version' Cargo.toml | head -1 | sed 's/.*= *"\(.*\)".*/\1/')"
REL="release/kzip-${VERSION}-windows-x86_64"
ZIP="release/kzip-${VERSION}-windows-x86_64.zip"

echo "== Building release binaries (zip-sys cdylib + ziptools) =="
cargo build --release --package zip-sys --package ziptools

echo "== Packaging into $REL =="
rm -rf "$REL" "$ZIP"
mkdir -p "$REL"
# kzip-branded release filenames (ABI symbols remain libzip-compatible zip_*).
cp target/release/zip.dll "$REL/kzip.dll"
cp target/release/zipcmp.exe "$REL/kzipcmp.exe"
cp crates/zip-sys/include/zip.h "$REL/kzip.h"
cp LICENSE README.md "$REL/"

# Optional code-signing.
if [ -n "${1:-}" ]; then
  echo "== Code-signing binaries =="
  bash scripts/sign-release.sh "$1" "${2:-}" "$ROOT/$REL"
fi

echo "== Creating ZIP =="
powershell -Command "Compress-Archive -Path '$REL/*' -DestinationPath '$ZIP' -Force"

echo "== Verifying ZIP =="
unzip -l "$ZIP" || tar -tf "$ZIP"

echo "== Release package: $ZIP =="

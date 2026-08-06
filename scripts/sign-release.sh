#!/usr/bin/env bash
# Code-sign the kzip release binaries (zip.dll, zipcmp.exe) with a code-signing
# certificate, then timestamp the signature.
#
# Usage:
#   bash scripts/sign-release.sh <path-to-pfx> [password] [target-dir]
#
#   <path-to-pfx>  path to the code-signing certificate (.pfx)
#   [password]     certificate password (default: $KZIP_CERT_PASSWORD)
#   [target-dir]   directory containing the binaries to sign
#                  (default: release/kzip-<version>-windows-x86_64)
#
# Environment:
#   KZIP_CERT_PASSWORD  certificate password (used if [password] arg omitted)
#   KZIP_TIMESTAMP_URL  RFC3161 timestamp server (default: DigiCert)
#
# Requires: signtool.exe (Windows SDK) — auto-detected under the Windows Kits dir.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PFX="${1:?usage: sign-release.sh <pfx> [password] [target-dir]}"
PASS="${2:-${KZIP_CERT_PASSWORD:-}}"
TARGET="${3:-}"

# Locate signtool.exe in the Windows SDK.
SIGNTOOL=""
for kit in "/c/Program Files (x86)/Windows Kits/10/bin"/*/x64/signtool.exe; do
  [ -f "$kit" ] && SIGNTOOL="$kit"
done
if [ -z "$SIGNTOOL" ]; then
  echo "!! signtool.exe not found under Windows Kits. Install the Windows SDK." >&2
  exit 1
fi
echo "Using signtool: $SIGNTOOL"

# Resolve the target directory (default: the packaged release dir).
if [ -z "$TARGET" ]; then
  VERSION="$(grep -E '^version' "$ROOT/Cargo.toml" | head -1 | sed 's/.*= *"\(.*\)".*/\1/')"
  TARGET="$ROOT/release/kzip-${VERSION}-windows-x86_64"
fi
[ -d "$TARGET" ] || { echo "!! target dir not found: $TARGET" >&2; exit 1; }

TS="${KZIP_TIMESTAMP_URL:-http://timestamp.digicert.com}"

sign() {
  local f="$1"
  echo "== Signing $f =="
  if [ -n "$PASS" ]; then
    "$SIGNTOOL" sign /f "$PFX" /p "$PASS" /fd SHA256 /tr "$TS" /td SHA256 /v "$f"
  else
    "$SIGNTOOL" sign /f "$PFX" /fd SHA256 /tr "$TS" /td SHA256 /v "$f"
  fi
}

for bin in "$TARGET"/kzip.dll "$TARGET"/kzipcmp.exe; do
  [ -f "$bin" ] && sign "$bin"
done

echo "== Verifying signatures =="
"$SIGNTOOL" verify /pa /v "$TARGET"/kzip.dll
"$SIGNTOOL" verify /pa /v "$TARGET"/kzipcmp.exe

echo "== Done. Signed binaries in: $TARGET =="

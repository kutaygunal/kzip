# C libzip baseline build record (Phase 0)

Recorded: 2026-08-04
Source: nih-at/libzip, commit `4cd7310` (v1.11.4)
Built from: `libzip/` (shallow clone)

## Toolchain
- CMake 3.31.1
- Generator: Visual Studio 17 2022 (x64)
- Compiler: MSVC 14.44.35207 (VS 2022 Community)
- vcpkg toolchain: `C:/src/vcpkg/scripts/buildsystems/vcpkg.cmake`
- vcpkg packages: zlib, bzip2 (used)

## Configure options
- `-DBUILD_SHARED_LIBS=ON`
- `-DCMAKE_BUILD_TYPE=Release`
- `-DENABLE_ZSTD=OFF -DENABLE_LZMA=OFF`  (deps not yet installed in vcpkg)
- `-DENABLE_OPENSSL=OFF -DENABLE_GNUTLS=OFF -DENABLE_COMMONCRYPTO=OFF`
  (Windows cryptography used for WinZip AES instead)
- `-DBUILD_REGRESS=OFF -DBUILD_OSSFUZZ=OFF`

## Artifacts
- `libzip/build/lib/Release/zip.dll`, `zip.lib`  (copied to `libs/c/`)
- Runtime deps copied alongside: `zlib1.dll`, `bz2.dll`

## Enabled codec matrix
| Codec   | Status | Backend |
|---------|--------|---------|
| DEFLATE | on     | zlib (vcpkg) |
| Bzip2   | on     | vcpkg bzip2 |
| Zstd    | off    | not yet in vcpkg |
| LZMA/XZ | off    | not yet in vcpkg |
| WinZip AES | on  | Windows BCrypt |

## Baseline result
Harness run against `libs/c/zip.dll` over `data/corpus/` → `results/c-baseline.json`.

## Next baseline steps
- Install `zstd` + `lzma` via vcpkg and rebuild C libzip for full codec parity.
- Generate a larger/mixed corpus (many files, mixed sizes, deep nesting).

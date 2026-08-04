/* zip.h — libzip-compatible subset exported by zip-sys (the Rust cdylib).
 *
 * The full libzip API is ~139 functions; this header mirrors ONLY the subset
 * actually exported by the Rust crate (read path + stat + error + version).
 * Regenerate with `scripts/gen-zip-h.sh` when cbindgen is installed; otherwise
 * this file is the source of truth and must be kept in sync with
 * crates/zip-sys/src/lib.rs.
 *
 * Symbols COMPLETE in this header:
 *   zip_open, zip_close, zip_get_num_entries, zip_get_name,
 *   zip_strerror, zip_file_strerror,
 *   zip_fopen, zip_fopen_index, zip_fread, zip_fclose,
 *   zip_stat, zip_stat_index, zip_stat_init, zip_libzip_version
 *
 * STUBBED / DEFERRED (not yet exported): write/edit path, encryption,
 * progress/cancel, source-construction APIs. See docs/ABI.md.
 */
#ifndef ZIP_H
#define ZIP_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* libzip scalar typedefs. */
typedef int64_t zip_int64_t;
typedef uint64_t zip_uint64_t;
typedef uint32_t zip_uint32_t;
typedef uint16_t zip_uint16_t;
typedef uint16_t zip_flags_t;

/* Opaque handles. */
typedef struct zip zip_t;
typedef struct zip_file zip_file_t;

/* zip_stat_t layout (matches crates/zip-sys/src/lib.rs `zip_stat`). */
typedef struct zip_stat {
    zip_uint64_t valid;
    const char *name;
    zip_uint64_t index;
    zip_uint64_t size;
    zip_uint64_t comp_size;
    int64_t mtime;
    zip_uint32_t crc;
    zip_uint16_t comp_method;
    zip_uint16_t encryption_method;
} zip_stat_t;

/* Error codes (ZIP_ER_*) — the subset mapped by zip-core. Numeric values are
 * stable and match libzip's zip_err_str.c. */
enum {
    ZIP_ER_OK = 0,
    ZIP_ER_MULTIDISK = 1,
    ZIP_ER_RENAME = 2,
    ZIP_ER_CLOSE = 3,
    ZIP_ER_SEEK = 4,
    ZIP_ER_READ = 5,
    ZIP_ER_WRITE = 6,
    ZIP_ER_CRC = 7,
    ZIP_ER_ZIPCLOSED = 8,
    ZIP_ER_NOENT = 9,
    ZIP_ER_EXISTS = 10,
    ZIP_ER_OPEN = 11,
    ZIP_ER_TMPOPEN = 12,
    ZIP_ER_ZLIB = 13,
    ZIP_ER_MEMORY = 14,
    ZIP_ER_CHANGED = 15,
    ZIP_ER_COMPNOTSUPP = 16,
    ZIP_ER_EOF = 17,
    ZIP_ER_INVAL = 18,
    ZIP_ER_NOZIP = 19,
    ZIP_ER_INTERNAL = 20,
    ZIP_ER_INCONS = 21,
    ZIP_ER_REMOVE = 22,
    ZIP_ER_DELETED = 23,
    ZIP_ER_ENCRNOTSUPP = 24,
    ZIP_ER_RDONLY = 25,
    ZIP_ER_NOPASSWD = 26,
    ZIP_ER_WRONGPASSWD = 27,
    ZIP_ER_OPNOTSUPP = 28,
    ZIP_ER_INUSE = 29,
    ZIP_ER_TELL = 30,
    ZIP_ER_COMPRESSED_DATA = 31,
    ZIP_ER_CANCELLED = 32,
    ZIP_ER_DATA_DESCRIPTOR = 33,
    ZIP_ER_WRONZIP = 34,
};

/* ---- archive lifecycle ---- */

/* Opens the archive at `path` for reading. On success returns a non-NULL
 * handle and sets *errorp = 0; on failure returns NULL and sets *errorp to a
 * ZIP_ER_* code. `flags` is currently ignored (read-only).
 *
 * The archive is read into memory and served from a contiguous buffer source.
 * A single handle may be shared for concurrent read operations (zip_fopen /
 * zip_fread) across threads; zip_close must not race other calls on the same
 * handle. */
zip_t *zip_open(const char *path, int flags, int *errorp);

/* Releases an archive opened by zip_open. Returns 0. */
int zip_close(zip_t *);

/* Number of entries, or -1 on error. */
zip_int64_t zip_get_num_entries(zip_t *, zip_flags_t flags);

/* Name of entry `index`, or NULL if out of range. Valid until zip_close. */
const char *zip_get_name(zip_t *, zip_uint64_t index, zip_flags_t flags);

/* ---- error reporting ---- */

/* Last error message for the archive; valid until the next call on this
 * handle. */
const char *zip_strerror(zip_t *);

/* Last error message for an open file handle. */
const char *zip_file_strerror(zip_file_t *);

/* ---- entry reading ---- */

/* Open entry `name` for reading. Returns an opaque handle or NULL. Must be
 * released with zip_fclose. */
zip_file_t *zip_fopen(zip_t *, const char *name, zip_flags_t flags);

/* Open entry `index` for reading. */
zip_file_t *zip_fopen_index(zip_t *, zip_uint64_t index, zip_flags_t flags);

/* Read up to `nbytes` bytes. Returns bytes read, 0 at EOF, or -1 on error. */
zip_int64_t zip_fread(zip_file_t *, void *buf, zip_uint64_t nbytes);

/* Closes an open entry handle. Returns 0. */
int zip_fclose(zip_file_t *);

/* ---- stat path ---- */

/* Fill `sb` with stat data for entry `name`. Returns 0 or -1. */
int zip_stat(zip_t *, const char *fname, zip_flags_t flags, zip_stat_t *sb);

/* Fill `sb` with stat data for entry `index`. Returns 0 or -1. */
int zip_stat_index(zip_t *, zip_uint64_t index, zip_flags_t flags,
                   zip_stat_t *sb);

/* Zero-initialize a zip_stat_t. */
void zip_stat_init(zip_stat_t *sb);

/* ---- version ---- */

/* libzip-compatible version string (static, never freed). */
const char *zip_libzip_version(void);

#ifdef __cplusplus
}
#endif

#endif /* ZIP_H */

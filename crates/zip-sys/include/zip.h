/* zip.h — libzip-compatible subset exported by zip-sys (the Rust cdylib).
 *
 * The full libzip API is ~139 functions; this header mirrors ONLY the subset
 * actually exported by the Rust crate (read path + stat + error + version +
 * write/edit path + error object + fseek + method queries + comment/extra-field
 * read). Regenerate with `scripts/gen-zip-h.sh` when cbindgen is installed;
 * otherwise this file is the source of truth and must be kept in sync with
 * crates/zip-sys/src/lib.rs.
 *
 * Symbols COMPLETE in this header:
 *   zip_open, zip_close, zip_get_num_entries, zip_get_name,
 *   zip_strerror, zip_file_strerror, zip_name_locate,
 *   zip_fopen, zip_fopen_index, zip_fread, zip_fclose,
 *   zip_stat, zip_stat_index, zip_stat_init, zip_libzip_version,
 *   zip_file_add, zip_dir_add, zip_delete, zip_rename, zip_file_replace,
 *   zip_discard, zip_source_buffer, zip_source_free,
 *   zip_get_error, zip_error_init, zip_error_init_with_code, zip_error_clear,
 *   zip_error_set, zip_error_strerror, zip_error_code_zip, zip_error_code_system,
 *   zip_error_fini, zip_error_to_str, zip_error_system_type, zip_error_get,
 *   zip_error_set_from_source,
 *   zip_fseek, zip_ftell, zip_file_is_seekable,
 *   zip_compression_method_supported, zip_encryption_method_supported,
 *   zip_get_archive_comment, zip_file_get_comment,
 *   zip_file_extra_fields_count, zip_file_extra_fields_count_by_id,
 *   zip_file_extra_field_get, zip_file_extra_field_get_by_id
 *
 * STUBBED / DEFERRED (not yet exported): encryption, full zip_source_* streaming
 * API, progress/cancel, comment/extra-field WRITE, zip_unchange*. See docs/ABI.md.
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
typedef uint8_t zip_uint8_t;
typedef int8_t zip_int8_t;
typedef int16_t zip_int16_t;
typedef int32_t zip_int32_t;
typedef uint16_t zip_flags_t;

/* Opaque handles. */
typedef struct zip zip_t;
typedef struct zip_file zip_file_t;
typedef struct zip_source zip_source_t;

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

/* zip_error_t layout (matches crates/zip-sys/src/lib.rs `zip_error`). */
typedef struct zip_error {
    int zip_err;
    int sys_err;
    char *str;
} zip_error_t;

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

/* zip_open flags. */
#define ZIP_CREATE 1
#define ZIP_EXCL 2
#define ZIP_CHECKCONS 4
#define ZIP_TRUNCATE 8
#define ZIP_RDONLY 16

/* zip_file_add flag. */
#define ZIP_FL_OVERWRITE 8192u

/* zip_error_system_type return values. */
#define ZIP_ET_NONE 0
#define ZIP_ET_SYS 1

/* Compression methods. */
#define ZIP_CM_DEFAULT (-1)
#define ZIP_CM_STORE 0
#define ZIP_CM_DEFLATE 8
#define ZIP_CM_BZIP2 12

/* Encryption methods. */
#define ZIP_EM_NONE 0

/* ---- archive lifecycle ---- */

/* Opens the archive at `path`. With ZIP_CREATE a new archive is created if the
 * file does not exist; with ZIP_TRUNCATE existing content is discarded. On
 * success returns a non-NULL handle and sets *errorp = 0; on failure returns
 * NULL and sets *errorp to a ZIP_ER_* code. */
zip_t *zip_open(const char *path, int flags, int *errorp);

/* Releases an archive. If it was opened for writing or has pending changes,
 * the archive is materialized and written to its path first. Returns 0. */
int zip_close(zip_t *);

/* Discards all pending changes and frees the handle without writing. */
void zip_discard(zip_t *);

/* Number of entries, or -1 on error. */
zip_int64_t zip_get_num_entries(zip_t *, zip_flags_t flags);

/* Name of entry `index`, or NULL if out of range. Valid until zip_close. */
const char *zip_get_name(zip_t *, zip_uint64_t index, zip_flags_t flags);

/* Index of the first entry named `name`, or -1 (with the archive error set to
 * ZIP_ER_NOENT) if not found. */
zip_int64_t zip_name_locate(zip_t *, const char *name, zip_flags_t flags);

/* ---- error reporting ---- */

/* Last error message for the archive; valid until the next call on this
 * handle. */
const char *zip_strerror(zip_t *);

/* Last error message for an open file handle. */
const char *zip_file_strerror(zip_file_t *);

/* Pointer to the archive's zip_error_t, valid until the next error is set. */
zip_error_t *zip_get_error(zip_t *);

/* Clear the archive's error. */
void zip_error_clear(zip_t *);

/* Caller-owned zip_error_t helpers. */
void zip_error_init(zip_error_t *);
void zip_error_init_with_code(zip_error_t *, int);
void zip_error_set(zip_error_t *, int, int);
void zip_error_set_from_source(zip_error_t *, zip_source_t *);
const char *zip_error_strerror(zip_error_t *);
int zip_error_code_zip(const zip_error_t *);
int zip_error_code_system(const zip_error_t *);
int zip_error_system_type(const zip_error_t *);
void zip_error_fini(zip_error_t *);
int zip_error_to_str(char *, zip_uint64_t, int, int);
void zip_error_get(zip_t *, int *, int *);

/* ---- entry reading ---- */

/* Open entry `name` for reading. Returns an opaque handle or NULL. Must be
 * released with zip_fclose. */
zip_file_t *zip_fopen(zip_t *, const char *name, zip_flags_t flags);

/* Open entry `index` for reading. */
zip_file_t *zip_fopen_index(zip_t *, zip_uint64_t index, zip_flags_t flags);

/* Open entry `name` for reading, decrypting with `password`. Returns an
 * opaque handle or NULL. Must be released with zip_fclose. */
zip_file_t *zip_fopen_encrypted(zip_t *, const char *name, zip_flags_t flags,
                                const char *password);

/* Open entry `index` for reading, decrypting with `password`. */
zip_file_t *zip_fopen_index_encrypted(zip_t *, zip_uint64_t index,
                                      zip_flags_t flags,
                                      const char *password);

/* Read up to `nbytes` bytes. Returns bytes read, 0 at EOF, or -1 on error. */
zip_int64_t zip_fread(zip_file_t *, void *buf, zip_uint64_t nbytes);

/* Closes an open entry handle. Returns 0. */
int zip_fclose(zip_file_t *);

/* Seek within an open entry (whence = SEEK_SET/SEEK_CUR/SEEK_END). Returns 0
 * or -1. */
zip_int8_t zip_fseek(zip_file_t *, zip_int64_t offset, int whence);

/* Current read position within an open entry, or -1 on error. */
zip_int64_t zip_ftell(zip_file_t *);

/* 1 if the open entry is seekable, 0 otherwise. */
int zip_file_is_seekable(zip_file_t *);

/* ---- stat path ---- */

/* Fill `sb` with stat data for entry `name`. Returns 0 or -1. */
int zip_stat(zip_t *, const char *fname, zip_flags_t flags, zip_stat_t *sb);

/* Fill `sb` with stat data for entry `index`. Returns 0 or -1. */
int zip_stat_index(zip_t *, zip_uint64_t index, zip_flags_t flags,
                   zip_stat_t *sb);

/* Zero-initialize a zip_stat_t. */
void zip_stat_init(zip_stat_t *sb);

/* ---- encryption ---- */

/* Set the default password used to decrypt encrypted entries (and to encrypt
 * on write). Returns 0 or -1. */
int zip_set_default_password(zip_t *, const char *password);

/* Set the encryption method for entry `index` (applied on zip_close).
 * ZIP_EM_NONE (0) and ZIP_EM_TRAD_PKWARE (1) are supported. Returns 0 or -1. */
int zip_file_set_encryption(zip_t *, zip_uint64_t index, zip_uint16_t method);

/* Traditional PKWARE (ZipCrypto) encryption method value. */
#define ZIP_EM_TRAD_PKWARE 1u

/* ---- write / edit path ---- */

/* Create a buffer-backed source from data[0..len] (copied; freep ignored). */
zip_source_t *zip_source_buffer(zip_t *, const void *data, zip_uint64_t len,
                                int freep);

/* Release a source created by zip_source_buffer. */
void zip_source_free(zip_source_t *);

/* Add a new entry from `source`, returning its index or -1. With
 * ZIP_FL_OVERWRITE an existing entry of the same name is replaced. */
zip_int64_t zip_file_add(zip_t *, const char *name, zip_source_t *,
                         zip_flags_t flags);

/* Add a directory entry named `name`, returning its index or -1. */
zip_int64_t zip_dir_add(zip_t *, const char *name, zip_flags_t flags);

/* Mark the entry at `index` for deletion (applied on close). */
int zip_delete(zip_t *, zip_uint64_t index);

/* Rename the entry at `index` to `name` (applied on close). */
int zip_rename(zip_t *, zip_uint64_t index, const char *name);

/* Replace the entry at `index` with the data from `source` (applied on close). */
int zip_file_replace(zip_t *, zip_uint64_t index, zip_source_t *,
                     zip_flags_t flags);

/* ---- method-support queries ---- */

/* 1 if compression `method` is supported for compression (compress != 0) or
 * decompression (compress == 0). */
int zip_compression_method_supported(zip_int32_t method, int compress);

/* 1 if encryption `method` is supported for encoding (encode != 0) or
 * decoding (encode == 0). */
int zip_encryption_method_supported(zip_uint16_t method, int encode);

/* ---- comments & extra fields (read) ---- */

/* Archive (EOCD) comment, or NULL. If lenp is non-null, the length is written
 * to it. */
const char *zip_get_archive_comment(zip_t *, int *lenp, zip_flags_t flags);

/* Comment of the entry at `index`, or NULL. If lenp is non-null, the length is
 * written to it. */
const char *zip_file_get_comment(zip_t *, zip_uint64_t index, zip_uint32_t *lenp,
                                 zip_flags_t flags);

/* Legacy alias for zip_file_get_comment (identical signature). */
const char *zip_get_file_comment(zip_t *, zip_uint64_t index, zip_uint32_t *lenp,
                                 zip_flags_t flags);

/* Number of extra fields of the entry at `index`, or -1 on error. */
zip_int16_t zip_file_extra_fields_count(zip_t *, zip_uint64_t index,
                                        zip_flags_t flags);

/* Number of extra fields with id `id` of the entry at `index`, or -1. */
zip_int16_t zip_file_extra_fields_count_by_id(zip_t *, zip_uint64_t index,
                                              zip_uint16_t id, zip_flags_t flags);

/* Pointer to the `idx`-th extra field of the entry at `index`, or NULL. If
 * idxp/lenp are non-null, the field's index/length are written to them. */
const zip_uint8_t *zip_file_extra_field_get(zip_t *, zip_uint64_t index,
                                            zip_uint16_t id, zip_uint16_t *idxp,
                                            zip_uint16_t *lenp, zip_flags_t flags);

/* Pointer to the `idx`-th extra field with id `id` of the entry at `index`, or
 * NULL. If lenp is non-null, the field's length is written to it. */
const zip_uint8_t *zip_file_extra_field_get_by_id(zip_t *, zip_uint64_t index,
                                                   zip_uint16_t id, zip_uint16_t idx,
                                                   zip_uint16_t *lenp,
                                                   zip_flags_t flags);

/* ---- version ---- */

/* libzip-compatible version string (static, never freed). */
const char *zip_libzip_version(void);

#ifdef __cplusplus
}
#endif

#endif /* ZIP_H */

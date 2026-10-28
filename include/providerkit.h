/* ProviderKit.dll — a file provider adapter SDK behind a
 * small C ABI: browse local disks, network shares, ZIP and
 * TAR archives, in-memory stores and custom providers
 * through ONE interface.
 *
 *   char *out;
 *   pv_list("zip:///C:/data/a.zip!/docs", &out);
 *   printf("%s\n", out); pv_free_string(out);
 *   pv_read_to_file("zip:///C:/data/a.zip!/docs/readme.txt",
 *                   "C:/tmp/readme.txt");
 *   pv_copy("file:///C:/tmp/readme.txt", "mem://scratch/docs/readme.txt");
 *
 * URI GRAMMAR (exact). Every operation takes a URI:
 *
 *   <scheme>://<location>
 *
 *   scheme   lowercase letter, then lowercase letters,
 *            digits, '+', '-' or '.'. Built-ins: file,
 *            zip, tar, mem. Anything else must have been
 *            registered with pv_register_provider, or the
 *            call fails with PV_ERR_UNKNOWN_PROVIDER (8).
 *
 *   file:///abs/path          local absolute path
 *   file://host/share/path    network share (UNC)
 *        In the path portion, %XX percent-escapes are
 *        decoded ('+' is literal). On Windows,
 *        file:///C:/path means C:/path.
 *
 *   zip:///path/to/archive.zip!/inner/path
 *   tar:///path/to/archive.tar!/inner/path
 *        The archive path follows the file:// path rules
 *        above (including file://host/share archive paths
 *        written zip://host/share/a.zip!/inner). The '!'
 *        separates the archive from the inner path; both
 *        zip:///a.zip and zip:///a.zip! name the archive
 *        root. Inner paths are normalized: a leading '/'
 *        is optional, '.' and empty components are
 *        dropped, and '..' is REJECTED
 *        (PV_ERR_INVALID_ARG) — an inner path can never
 *        name something outside its archive.
 *
 *   mem://name/path
 *        An in-memory store called <name>; files are put
 *        there with pv_mem_add_from_file. Inner paths
 *        follow the same normalization rules as archives.
 *
 *   <custom>://anything
 *        Handed verbatim to the registered provider.
 *
 * OPERATIONS. Unless noted, JSON results are returned
 * through a char** out parameter with a PV_* code:
 *
 *   pv_list(uri, &out)
 *     {"entries":[{"name":"a.txt","path":"<full uri>",
 *       "is_dir":false,"size":123,"modified_unix":1700000000}],
 *      "provider":"file"}
 *     Direct children only. Entries are sorted by name.
 *     modified_unix is seconds since the Unix epoch (UTC),
 *     or null when the backend has no time for the entry.
 *     Listing a file, or a missing directory, is an error
 *     (PV_ERR_INVALID_ARG / PV_ERR_NOT_FOUND).
 *
 *   pv_stat(uri, &out)
 *     {"exists":true,"is_dir":false,"size":123,
 *      "modified_unix":1700000000,"provider":"file"}
 *     A MISSING target is not an error: exists is false,
 *     size 0, modified_unix null. Unusable input (unknown
 *     scheme, malformed URI, a missing or corrupt archive
 *     file) IS an error.
 *
 *   pv_read_to_file(uri, dest_local_path) -> PV_* code
 *     Writes the URI's content to a plain local path
 *     (created/truncated). Built-in file/zip/tar content
 *     is streamed in 64 KiB chunks — never held whole in
 *     memory. ZIP STORED entries are CRC-32 checked while
 *     streaming; a mismatch deletes the partial output
 *     and returns PV_ERR_INTEGRITY. mem content is in
 *     memory by definition of the store.
 *
 *   pv_read_text(uri, max_bytes) -> char* (caller-owned)
 *     The content as a NUL-terminated UTF-8 string, for
 *     small files. NULL on failure (last-error set):
 *     larger than max_bytes is PV_ERR_TOO_LARGE; content
 *     that is not valid UTF-8 is PV_ERR_PARSE.
 *
 *   pv_copy(src_uri, dst_uri) -> PV_* code
 *     Cross-provider copy of ONE file. Directories are
 *     PV_ERR_NOT_SUPPORTED in v0.1. Writable destinations
 *     in v0.1: file:// and mem:// ONLY. Copying INTO a
 *     zip or tar (file->zip included) returns
 *     PV_ERR_NOT_SUPPORTED — archives are read-only in
 *     v0.1 — as does copying to a custom provider.
 *     Copying a file onto itself is PV_ERR_INVALID_ARG.
 *
 *   pv_providers(&out)
 *     {"providers":[{"scheme":"file","provider":"file",
 *       "read":true,"write":true,"list":true}, ...]}
 *     Built-ins first (file, mem, tar, zip), then custom
 *     schemes sorted by name. For a custom provider,
 *     "read" is true when it supplied a read_to_file
 *     callback; "write" is always false in v0.1.
 *
 *   pv_mem_add_from_file(name, inner_path, local_path)
 *     Reads the local file and stores its bytes in the
 *     mem store <name> at <inner_path>, replacing any
 *     file already there. Creates the store on first use.
 *
 * ZIP SUPPORT (honest scope). The reader parses the End
 * Of Central Directory and the central directory.
 * STORED (method 0) entries: list, stat, read, copy —
 * fully supported, CRC-32 checked on read. DEFLATE
 * (method 8) entries: list and stat work; reading
 * returns PV_ERR_NOT_SUPPORTED with the message
 * "DEFLATE decompression is not built into ProviderKit
 * v0.1". Other methods, ZIP64 and multi-disk archives
 * also return PV_ERR_NOT_SUPPORTED. A file that is not
 * a ZIP, or is truncated, returns PV_ERR_INTEGRITY.
 *
 * TAR SUPPORT. Plain uncompressed TAR with ustar headers
 * (including the ustar prefix field): list, stat, read
 * and copy are fully supported. Header checksums are
 * verified (a corrupt header is PV_ERR_INTEGRITY).
 * Entries that are not regular files or directories
 * (links, devices, ...) can be listed/stated; reading
 * one returns PV_ERR_NOT_SUPPORTED. Base-256 numeric
 * fields are not supported. Compressed tarballs
 * (.tar.gz etc.) are NOT supported — there is no
 * decompression in v0.1.
 *
 * PLUGIN SDK — pv_register_provider(scheme, &callbacks,
 * user_data). The callback contract is EXACTLY this:
 *
 *   typedef struct PvProviderCallbacks {
 *       uint32_t struct_size;   // must be
 *                               // sizeof(PvProviderCallbacks)
 *       PvListFn       list;         // required
 *       PvStatFn       stat;         // required
 *       PvReadToFileFn read_to_file; // may be NULL
 *       PvFreeStringFn free_string;  // required
 *   } PvProviderCallbacks;
 *
 *   list(user_data, uri, &out_json) / stat(user_data, uri,
 *   &out_json): the uri is the full URI exactly as the
 *   caller passed it, NUL-terminated UTF-8. On PV_OK the
 *   callback stores in *out_json a NUL-terminated UTF-8
 *   JSON string that the PROVIDER allocated (for example
 *   with malloc/strdup or CString::into_raw). ProviderKit
 *   copies the string, then calls the provider's
 *   free_string(user_data, that_pointer) EXACTLY ONCE —
 *   free_string must release a string the same way it was
 *   allocated. On a nonzero return *out_json must be NULL
 *   and ProviderKit propagates that code. list JSON must
 *   be the pv_list shape (an object with an "entries"
 *   array; include "provider":"<scheme>"). stat JSON is
 *   {"exists":bool,"is_dir":bool,"size":uint,
 *    "modified_unix":int|null} — ProviderKit validates it
 *   and adds "provider" itself. Returning invalid JSON,
 *   or PV_OK with a NULL string, is PV_ERR_INTERNAL.
 *
 *   read_to_file(user_data, uri, dest_path): writes the
 *   URI's full content to the local file at dest_path
 *   (UTF-8, create/truncate), returns PV_OK or a PV_ERR_*
 *   code. When this callback is NULL, reads through the
 *   provider return PV_ERR_NOT_SUPPORTED.
 *
 *   Callbacks may be invoked from any thread the host
 *   calls on; they must be thread-safe if the host is.
 *   user_data is handed back verbatim to every callback,
 *   including free_string. A callback that panics/crashes
 *   is the provider's bug, though ProviderKit catches
 *   Rust-side unwinds from callbacks registered by Rust
 *   hosts. Registering a duplicate scheme, or one that
 *   collides with a built-in (file/zip/tar/mem), returns
 *   PV_ERR_DUPLICATE. pv_unregister_provider removes a
 *   custom registration (built-ins cannot be removed).
 *
 * ERRORS. Codes are distinct per failure kind (see the
 * enum below) and every failure also sets a thread-local
 * pv_last_error() message in plain words.
 *
 * THREADING. The built-in providers and the mem stores
 * are process-global and internally locked; all calls
 * are thread-safe. Registrations are process-global too.
 *
 * All strings are UTF-8, NUL-terminated. char* results
 * are caller-owned: free with pv_free_string().
 */
#ifndef PROVIDERKIT_H
#define PROVIDERKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(PV_BUILD)
#define PV_API __declspec(dllexport)
#elif defined(_WIN32)
#define PV_API __declspec(dllimport)
#else
#define PV_API
#endif

enum {
    PV_OK = 0,
    PV_ERR_INVALID_ARG = 1,      /* null pointer, empty/invalid URI part, listing a file, same-file copy */
    PV_ERR_NOT_SUPPORTED = 2,    /* DEFLATE read, copy into zip/tar/custom, directory copy, ZIP64, ... */
    PV_ERR_NOT_FOUND = 3,        /* missing file, missing archive entry, missing mem store/file */
    PV_ERR_ACCESS_DENIED = 4,    /* OS permission denied */
    PV_ERR_PLATFORM = 5,         /* other OS / I/O failure; see pv_last_error() */
    PV_ERR_INTERNAL = 6,         /* internal failure, or a provider broke its contract */
    PV_ERR_PARSE = 7,            /* content read as text is not valid UTF-8 */
    PV_ERR_UNKNOWN_PROVIDER = 8, /* no provider registered for the scheme */
    PV_ERR_DUPLICATE = 9,        /* scheme already registered, or collides with a built-in */
    PV_ERR_TOO_LARGE = 10,       /* content larger than pv_read_text's max_bytes */
    PV_ERR_INTEGRITY = 11        /* CRC-32 mismatch, corrupt ZIP/TAR structure */
};

PV_API const char *pv_version(void);
PV_API const char *pv_last_error(void);
PV_API int pv_last_error_code(void);
PV_API void pv_free_string(char *p);

/* List a directory URI as JSON (see above). */
PV_API int pv_list(const char *uri, char **out_json);

/* Stat one URI as JSON (see above). Missing != error. */
PV_API int pv_stat(const char *uri, char **out_json);

/* Every registered provider + capabilities, as JSON. */
PV_API int pv_providers(char **out_json);

/* Write a URI's content to a plain local file path. */
PV_API int pv_read_to_file(const char *uri, const char *dest_local_path);

/* Read a small file as text. Caller-owned; NULL on error. */
PV_API char *pv_read_text(const char *uri, uint64_t max_bytes);

/* Copy ONE file between providers (see writability rules). */
PV_API int pv_copy(const char *src_uri, const char *dst_uri);

/* Store a local file's bytes in the mem store <name> at
 * <inner_path> (replacing any file already there). */
PV_API int pv_mem_add_from_file(const char *name, const char *inner_path,
                                const char *local_path);

/* --- Plugin SDK (contract: see the long comment above) --- */

typedef int (*PvListFn)(void *user_data, const char *uri, char **out_json);
typedef int (*PvStatFn)(void *user_data, const char *uri, char **out_json);
typedef int (*PvReadToFileFn)(void *user_data, const char *uri,
                               const char *dest_path);
typedef void (*PvFreeStringFn)(void *user_data, char *p);

typedef struct PvProviderCallbacks {
    uint32_t struct_size; /* sizeof(PvProviderCallbacks) */
    PvListFn list;
    PvStatFn stat;
    PvReadToFileFn read_to_file; /* may be NULL */
    PvFreeStringFn free_string;
} PvProviderCallbacks;

PV_API int pv_register_provider(const char *scheme,
                                const PvProviderCallbacks *callbacks,
                                void *user_data);
PV_API int pv_unregister_provider(const char *scheme);

#ifdef __cplusplus
}
#endif
#endif /* PROVIDERKIT_H */

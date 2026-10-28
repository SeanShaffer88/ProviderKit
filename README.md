# ProviderKit

## TL;DR

One small, zero-dependency DLL that lets a program browse and read files the same way everywhere: a folder on a local disk, a network share, entries inside a ZIP or TAR archive, an in-memory store, or a custom backend you register yourself — all through URIs like `zip:///C:/data/a.zip!/docs/readme.txt` and the same five calls (`pv_list`, `pv_stat`, `pv_read_to_file`, `pv_read_text`, `pv_copy`).

```c
#include "providerkit.h"
#include <stdio.h>

int main(void) {
    char *out;
    pv_list("zip:///C:/data/a.zip!/docs", &out);
    printf("%s\n", out);                 /* {"entries":[...],"provider":"zip"} */
    pv_free_string(out);

    pv_read_to_file("zip:///C:/data/a.zip!/docs/readme.txt", "C:/tmp/readme.txt");
    pv_copy("file:///C:/tmp/readme.txt", "mem://scratch/docs/readme.txt");
    char *text = pv_read_text("mem://scratch/docs/readme.txt", 65536);
    pv_free_string(text);
    return 0;
}
```

## Features

- **One interface, many backends.** `file://` (local disks and `file://host/share` network shares), `zip://` and `tar://` (entries inside archives), `mem://` (an in-memory store), and any scheme you register. Operations dispatch on the scheme; the calling code does not change.
- **Consistent JSON output.** `pv_list` returns direct children (`name`, full-URI `path`, `is_dir`, `size`, `modified_unix` or null) sorted by name, plus the provider name. `pv_stat` reports a missing target as `{"exists":false,...}` rather than an error. `pv_providers` lists every scheme with its `{read, write, list}` capabilities.
- **Cross-provider copy.** Copy a file from any readable provider to `file://` or `mem://` — for example extracting one archive entry straight into memory. Archives are read-only destinations in v0.1 and say so with `PV_ERR_NOT_SUPPORTED`.
- **Streaming reads.** Built-in file, ZIP and TAR content is copied in 64 KiB chunks, never held whole in memory (the `mem://` store is in memory by definition).
- **A real ZIP reader, honestly scoped.** End Of Central Directory + central directory parsing, STORED entries read fully with a CRC-32 check on every extraction, DEFLATE entries listed and stated but not read in v0.1 (see Limitations).
- **A real TAR reader.** Plain uncompressed TAR with ustar headers, header checksums verified, list/stat/read fully supported.
- **A plugin SDK.** Register a scheme with four C callbacks (list, stat, optional read_to_file, free_string) and it works through every `pv_*` operation, including cross-provider copies out of it. The exact callback contract is documented in `include/providerkit.h`.
- **Distinct errors, plain messages.** Unknown scheme, malformed URI, not found, too large, CRC mismatch and "not supported" are different codes (`PV_ERR_*`), each with a thread-local `pv_last_error()` message.
- **Zero dependencies.** Rust, hand-written ZIP/TAR/CRC-32/JSON code, no crates.

## The URI grammar

```
file:///abs/path            file://host/share/path
zip:///path/to/a.zip!/inner/path
tar:///path/to/a.tar!/inner/path
mem://storename/path
<custom>://anything         (scheme must be registered)
```

The `!` separates an archive from the path inside it; `zip:///a.zip` and `zip:///a.zip!` both name the archive root. Inner paths may omit the leading `/`; `.` components are dropped and `..` is rejected, so an inner path can never point outside its container. In `file://` paths, `%XX` percent-escapes are decoded.

Registering a custom provider from C:

```c
#include "providerkit.h"
#include <stdlib.h>
#include <string.h>

static int demo_list(void *ud, const char *uri, char **out) {
    const char *json = "{\"entries\":[],\"provider\":\"demo\"}";
    size_t n = strlen(json) + 1;
    char *copy = (char *)malloc(n);
    memcpy(copy, json, n);
    *out = copy;
    return PV_OK;
}
static int demo_stat(void *ud, const char *uri, char **out) {
    const char *json = "{\"exists\":false,\"is_dir\":false,\"size\":0,\"modified_unix\":null}";
    size_t n = strlen(json) + 1;
    char *copy = (char *)malloc(n);
    memcpy(copy, json, n);
    *out = copy;
    return PV_OK;
}
static void demo_free(void *ud, char *p) { free(p); }

int main(void) {
    PvProviderCallbacks cb;
    memset(&cb, 0, sizeof cb);
    cb.struct_size = sizeof cb;
    cb.list = demo_list;
    cb.stat = demo_stat;
    cb.read_to_file = NULL;      /* reads return PV_ERR_NOT_SUPPORTED */
    cb.free_string = demo_free;  /* releases the strings list/stat return */
    pv_register_provider("demo", &cb, NULL);

    char *out;
    pv_list("demo://anything", &out);   /* routed to demo_list */
    pv_free_string(out);
    pv_unregister_provider("demo");
    return 0;
}
```

The contract in one paragraph: on `PV_OK`, list/stat store a provider-allocated NUL-terminated JSON string in `*out_json`; ProviderKit copies it and calls the provider's `free_string` exactly once to release the original. On a nonzero return, `*out_json` must be NULL and the code is propagated to the caller. Full details are in the header.

## Building

```
cargo build --release     # providerkit.dll / libproviderkit.so / static lib
cargo test --locked       # the full test suite
```

Rust 1.80+, edition 2021, crate types `cdylib` + `rlib` + `staticlib`. No dependencies to download.

## Tests

20 tests, all fixtures built in-test (a temporary directory tree; a ZIP archive assembled byte-by-byte with stored entries, a DEFLATE-header entry and a deliberately wrong CRC; a TAR archive assembled header-by-header):

- URI grammar: absolute and UNC `file://` paths, percent-decoding, archive root forms, `..` rejection, unknown scheme, invalid schemes.
- File backend: list ordering and fields, stat of present/missing paths, read_text cap, list-on-a-file error, same-file copy refusal, directory-copy refusal.
- ZIP: list/stat of stored and DEFLATE entries (including DOS timestamps), stored extraction, DEFLATE read returning `PV_ERR_NOT_SUPPORTED`, CRC mismatch returning `PV_ERR_INTEGRITY` and removing the partial output, missing/corrupt archive errors.
- TAR: list/stat/read, ustar directory entries, a corrupted header caught by its checksum.
- mem and cross-provider copies: file → mem → file round trip, zip → mem, copy into zip/tar refused, providers list capabilities.
- Custom providers, registered both through the Rust API and through the C ABI (`pv_register_provider`): list/stat routing, stat JSON re-emitted with the provider name, copy out to a file, duplicate-scheme and built-in-collision errors, unregister behaviour, and a provider without a read callback reporting `PV_ERR_NOT_SUPPORTED` on read.

## Limitations

- **DEFLATE is not decompressed in v0.1.** ZIP entries using method 8 can be listed and stated; reading one returns `PV_ERR_NOT_SUPPORTED` with a message saying exactly that. No compressed TAR variants (`.tar.gz`, `.tar.xz`) are supported either.
- **Archives are read-only.** Copying or writing into a `zip://` or `tar://` URI returns `PV_ERR_NOT_SUPPORTED`. Custom providers are also read-only as copy destinations in v0.1.
- **No ZIP64, multi-disk ZIP, encrypted ZIP, or other compression methods** (bzip2, LZMA, ...). Each returns `PV_ERR_NOT_SUPPORTED` or, for a file that is not a ZIP at all, `PV_ERR_INTEGRITY`.
- **Copies are single files.** There is no recursive directory copy in v0.1; copying a directory returns `PV_ERR_NOT_SUPPORTED`.
- **TAR limits:** header checksums and the ustar prefix field are supported; base-256 numeric fields are not, and non-regular entries (links, devices) can be listed but not read.
- **Copying into `mem://` loads the file into memory** — that is what the store is. Copying a very large file there needs that much RAM.
- **`pv_read_text` is for small files by design** — it refuses anything over the caller's `max_bytes` cap and anything that is not valid UTF-8. Use `pv_read_to_file` for binary or large content.
- Timestamps inside ZIP are MS-DOS local-time fields with no timezone; ProviderKit converts them as if they were UTC, which is the only consistent choice available.

## License

MIT — see `LICENSE`. Part of the WinDevKit family; see `AGENTS.md` for build discipline, `ROADMAP.md` for what is next, `QUALITY.md` for the scored quality record, and `docs/PARITY.md` for the C ABI / Rust API parity table.

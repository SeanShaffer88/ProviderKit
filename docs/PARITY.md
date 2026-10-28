# ProviderKit — Parity

Every C ABI operation is a thin wrapper over the Rust (rlib) API in
`src/lib.rs`; there is no capability on one surface that the other lacks,
except the two C-only concerns (string ownership and the thread-local
last-error), noted at the end.

| Capability | C ABI (`include/providerkit.h`) | Rust API (`provider_kit::`) |
|---|---|---|
| Version | `pv_version()` | `VERSION` / `VERSION_MAJOR` / `VERSION_MINOR` / `VERSION_PATCH` |
| List a directory | `pv_list(uri, &out)` → JSON | `list(uri) -> PvResult<String>` |
| Stat one URI | `pv_stat(uri, &out)` → JSON | `stat(uri) -> PvResult<Stat>` (`Stat::to_json`) |
| Read to a local file | `pv_read_to_file(uri, dest)` | `read_to_file(uri, dest)` |
| Read small text | `pv_read_text(uri, max_bytes)` → `char*` | `read_text(uri, max_bytes) -> PvResult<String>` |
| Copy between providers | `pv_copy(src, dst)` | `copy(src, dst)` |
| Providers + capabilities | `pv_providers(&out)` → JSON | `providers_json() -> String` |
| Add a file to a mem store | `pv_mem_add_from_file(name, inner, local_path)` | `mem_add_from_file(name, inner, local_path)` |
| Register a custom provider | `pv_register_provider(scheme, &callbacks, user_data)` | `register_provider(scheme, &callbacks, user_data)` — same `ProviderCallbacks` struct, same callback types |
| Unregister a provider | `pv_unregister_provider(scheme)` | `unregister_provider(scheme)` |

Rust-only extras (no C counterpart, by design):

| Rust API | Why it is Rust-only |
|---|---|
| `mem_add_bytes(name, inner, &[u8])` | The C ABI moves bytes through files (`pv_mem_add_from_file`); a raw byte-slice entry point only makes sense in Rust. It is also how tests seed stores. |
| `parse_uri(uri) -> PvResult<ParsedUri>` | The URI parser is exposed for Rust hosts that want to inspect routing; C hosts get the same validation implicitly on every call. |
| `crc32(data)` / `crc32_update(state, data)` | Utility used by the ZIP reader; exposed for Rust hosts building archives in tests. |
| `normalize_inner(path)` | The inner-path normalizer, exposed so Rust hosts can predict how an inner path will be interpreted. |

C-only concerns:

| C ABI | Rust equivalent |
|---|---|
| `pv_free_string(p)` — frees caller-owned `char*` results | Rust results are owned `String`s; nothing to free. |
| `pv_last_error()` / `pv_last_error_code()` — thread-local error detail | Rust errors are the returned `(code, message)` pair in `PvResult<T>`; the FFI layer stores that same pair in the thread-local on failure. |

Error codes are identical on both surfaces: the `PV_ERR_*` values in the
header are the `ERR_*` constants in `src/lib.rs` (0–11, same meanings).

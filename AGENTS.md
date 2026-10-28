# ProviderKit — agent notes

- Zero dependencies is a design rule: the ZIP reader, TAR reader, CRC-32 and JSON engine are hand-written in `src/lib.rs` / `src/json.rs`. Do not add a dependency without a written justification.
- DEFLATE honesty is contractual: v0.1 lists/stats DEFLATE entries but reading one returns `PV_ERR_NOT_SUPPORTED` saying decompression is not built in. Never fake a read (no returning compressed bytes as content, no silent empty file).
- The plugin string contract is load-bearing: provider list/stat return provider-allocated strings; the library copies, then calls the provider's `free_string` exactly once — including on invalid UTF-8. Do not switch to library-allocator ownership without changing the header, the README example and both registration tests together.
- Inner paths (`zip`/`tar`/`mem`) are normalized by `normalize_inner` and `..` is rejected. Archive entry names are untrusted input: entries whose names fail normalization are unaddressable and skipped in listings.
- Stored-ZIP extraction is CRC-32 verified and a mismatch deletes the partial output file. Do not weaken that to a warning.
- Writability is a v0.1 promise, not an accident: only `file://` and `mem://` are copy destinations; zip/tar/custom destinations return `PV_ERR_NOT_SUPPORTED`. `copy` checks destination writability before touching the source so the error is not masked.
- Gate: WinDevKit `tools/gate.sh` (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real Windows build is the link gate.

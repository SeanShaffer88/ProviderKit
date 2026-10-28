# ProviderKit Roadmap

## v0.1.0 (current)
One URI interface over `file://` (local + UNC shares), `zip://` and `tar://` (hand-written readers; stored ZIP entries fully read with CRC-32 verification, DEFLATE entries list/stat only), `mem://` stores, and custom providers registered through the C callback SDK. Operations: list, stat, read_to_file (chunked), read_text (capped), cross-provider single-file copy to writable destinations, and a providers/capabilities listing. Distinct error codes per failure kind.

## Next, in order
1. **DEFLATE decompression** for ZIP entries (an in-crate inflate, verified against known vectors the way the family codec kits verify theirs), which would also unlock `.tar.gz` if a gzip wrapper follows it.
2. **Archive writing**: create/append for ZIP (stored first) and TAR, making archives writable copy destinations with the same URI grammar.
3. **Recursive copy and directory operations**: copy a directory tree between providers, plus create-directory and remove operations where the backend supports them.
4. **ZIP64 reading** and the ustar long-name (`L`) TAR entry, for archives past the current format limits.
5. **Streaming read handle**: an open/read/close API so hosts can consume large entries incrementally without a scratch file (today `pv_read_to_file` covers extraction; custom-provider `read_text` routes through one internally).

## Explicitly not planned
- Cloud storage backends in this crate: network providers belong behind the plugin SDK in the host's own code, with the host's own credentials and retry policy.
- A virtual filesystem / mount layer: ProviderKit is a library API, not a driver.

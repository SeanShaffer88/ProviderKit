# QUALITY — ProviderKit v0.1.0

Scored from evidence only (Elite §17). Gate numbers are from the actual
gate run on this machine (2026-10-28): WinDevKit `tools/gate.sh` —
**GATE GREEN**: `cargo fmt --check`, clippy with `-D warnings` on the host
target **and** on `x86_64-pc-windows-msvc`, `cargo check` for the Windows
target with all targets, and the full test suite **20/20 passed**.
`cargo build --release` also succeeds (cdylib + rlib + staticlib produced).
Coverage percentage is not measured (no coverage tool in the offline
toolchain); the test inventory below is the evidence instead.

Total: **87 / 100** — no dimension below 3, no Fail.

| Dimension | Score /5 | Evidence |
|---|---|---|
| Correctness | 5 | URI grammar tests (file/UNC/percent, archive roots, `..` rejection, unknown scheme); CRC-32 checked against the standard `"123456789" -> 0xCBF43926` vector; DOS-time conversion checked against 1980-01-01 UTC; ZIP and TAR fixtures are built byte-by-byte in-test and read back through the public operations. |
| Error handling | 4 | Distinct codes for invalid argument (1), not supported (2), not found (3), access denied (4), platform/IO (5), internal (6), parse (7), unknown provider (8), duplicate scheme (9), too large (10) and integrity (11), each tested at least once; corrupt ZIP, corrupt TAR header checksum, CRC mismatch, missing archive and missing mem store each produce their specific code. Not 5: OS error mapping beyond NotFound/PermissionDenied is a single platform bucket by design. |
| Tests | 4 | 20 tests: file tree list/stat/read/copy, ZIP stored + DEFLATE + CRC-mismatch fixtures, TAR fixture with checksum corruption, mem round trips (file→mem→file, zip→mem), custom provider through the Rust API and an FFI-level registration through the C ABI, providers/capabilities JSON. Not 5: no fuzzing of the archive parsers and no coverage measurement. |
| Honesty of scope | 5 | DEFLATE reads return `PV_ERR_NOT_SUPPORTED` naming the missing decompression (tested, message asserted); copies into zip/tar/custom are refused (tested); ZIP64/multi-disk/other methods are refused, never silently mishandled; all of it is stated in the header, README and ROADMAP. |
| Code structure | 4 | One core (`src/lib.rs`: URI parser, four backends, registry, operations), thin FFI (`src/ffi.rs`), family JSON engine (`src/json.rs`), one platform conversion (`src/platform.rs`). Clippy clean on both targets. Not 5: `lib.rs` is large (~2,400 lines with tests) and would split by backend in a future refactor. |
| Documentation | 4 | Header documents every export, the full URI grammar and the exact plugin callback contract (allocation/freeing, error propagation, struct_size); README has a TL;DR, a C example including registering a custom provider, build/test instructions and an explicit Limitations section; `docs/PARITY.md` maps every C call to its Rust counterpart. Not 5: no worked example program under `examples/` yet. |
| API consistency | 4 | All JSON-out operations use the same code + `char**` out shape; `pv_read_text` is the single documented exception (direct `char*`, per its contract). Stat-of-missing is `exists:false` on every backend, tested for file, zip and mem. |
| Performance | 3 | Built-in reads stream in 64 KiB chunks and ZIP/TAR content is never loaded whole (archive central/headers metadata excepted); ZIP local data is read through `Take`, TAR data is seek-skipped during scans. Not higher: no benchmarks were run, archive scans re-parse central/headers per operation, and copying into `mem://` is whole-file by definition — unmeasured, so scored conservatively. |
| Security / input safety | 4 | Inner paths reject `..`; archive entry names that fail normalization are unaddressable; stored-ZIP extraction is CRC-verified with partial output deleted on mismatch; provider callbacks are wrapped in `catch_unwind` and their returned strings are validated as UTF-8/JSON before use. Not 5: no fuzz corpus for hostile archives. |
| Platform reach | 4 | Pure `std` file APIs — the host suite and the Windows MSVC clippy/check passes are both green. On-Windows runtime behaviour of the built DLL is **Unverified** (no Windows machine run in this pass; check/clippy do not link, so the real Windows link remains CI's verdict). |

## Known limitations recorded as waivers

- DEFLATE decompression absent (v0.1 scope, documented everywhere a user
  could meet it) — roadmap item 1, not a silent gap.
- Windows runtime **Unverified** as noted above; the gate's Windows
  clippy/check steps are construction evidence only.

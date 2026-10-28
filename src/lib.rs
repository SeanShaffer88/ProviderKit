//! ProviderKit — one interface over many file backends.
//!
//! A URI names a location; the scheme names the provider:
//!
//! - `file://` — local disks and network shares.
//! - `zip://` / `tar://` — entries inside a ZIP / TAR archive
//!   stored on a local path (read-only in v0.1).
//! - `mem://` — an in-memory store filled through
//!   `pv_mem_add_from_file` (tests, demos, scratch space).
//! - Any other scheme — a provider registered at run time
//!   with `pv_register_provider` (the plugin SDK).
//!
//! Every operation (`list`, `stat`, `read_to_file`,
//! `read_text`, `copy`) dispatches on the scheme, so a host
//! browses an archive or a custom backend with exactly the
//! calls it uses for a local folder. The C ABI lives in
//! `ffi`; the JSON engine in `json`; the one time conversion
//! in `platform`. Zero dependencies: the ZIP reader, TAR
//! reader and CRC-32 below are hand-written in this file.

pub mod ffi;
pub mod json;
pub mod platform;

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_char, c_void};
use std::sync::{Mutex, OnceLock};

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/providerkit.h).
pub const PV_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;
/// No provider is registered for the URI's scheme.
pub const ERR_UNKNOWN_PROVIDER: i32 = 8;
/// The scheme is already registered (built-in or custom).
pub const ERR_DUPLICATE: i32 = 9;
/// Content is larger than the caller's stated cap.
pub const ERR_TOO_LARGE: i32 = 10;
/// Stored data failed an integrity check (CRC-32 mismatch,
/// corrupt archive structure, bad TAR header checksum).
pub const ERR_INTEGRITY: i32 = 11;

pub type PvResult<T> = Result<T, (i32, String)>;

fn err<T>(code: i32, msg: impl Into<String>) -> PvResult<T> {
    Err((code, msg.into()))
}
fn io_err<T>(what: &str, e: std::io::Error) -> PvResult<T> {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => ERR_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => ERR_ACCESS_DENIED,
        _ => ERR_PLATFORM,
    };
    Err((code, format!("{what}: {e}")))
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn opt_i64(v: Option<i64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3, as used by ZIP)
// ---------------------------------------------------------------------------

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        table
    })
}

/// Feed bytes into a running CRC-32 state (start at
/// `0xFFFF_FFFF`, finish by XOR-ing with `0xFFFF_FFFF`).
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    let table = crc_table();
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// One-shot CRC-32 of a byte string.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// A reader wrapper that computes CRC-32 as bytes pass
/// through, for verifying stored ZIP entries while streaming.
struct CrcReader<R: Read> {
    inner: R,
    crc: u32,
}
impl<R: Read> CrcReader<R> {
    fn new(inner: R) -> Self {
        CrcReader {
            inner,
            crc: 0xFFFF_FFFF,
        }
    }
    fn finish(&self) -> u32 {
        self.crc ^ 0xFFFF_FFFF
    }
}
impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc = crc32_update(self.crc, &buf[..n]);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// URI parsing
// ---------------------------------------------------------------------------

/// Decode `%XX` escapes in a path portion. `+` is literal.
fn percent_decode(s: &str) -> PvResult<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or((ERR_INVALID_ARG, "truncated % escape in path".to_string()))?;
            let hex = std::str::from_utf8(hex)
                .map_err(|_| (ERR_INVALID_ARG, "invalid % escape in path".to_string()))?;
            let v = u8::from_str_radix(hex, 16)
                .map_err(|_| (ERR_INVALID_ARG, "invalid % escape in path".to_string()))?;
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| (ERR_INVALID_ARG, "path is not valid UTF-8".to_string()))
}

/// Convert the location part of a `file://`-style URI to a
/// local path: `file:///abs/p` -> `/abs/p`;
/// `file://host/share/p` -> `//host/share/p` (a UNC path on
/// Windows). On Windows `file:///C:/p` -> `C:/p`.
fn location_to_path(loc: &str) -> PvResult<String> {
    if loc.is_empty() {
        return err(ERR_INVALID_ARG, "URI has an empty path");
    }
    let decoded = percent_decode(loc)?;
    // A Windows drive location (file://C:/a/b or file://C:\\a\\b)
    // is an absolute local path, not a host name: keep it as-is
    // instead of falling into the UNC branch below.
    {
        let b = decoded.as_bytes();
        if b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'/' || b[2] == b'\\')
        {
            return Ok(decoded);
        }
    }
    if decoded.starts_with('/') {
        // /C:/a/b is the file:/// spelling of a Windows drive
        // path; strip the leading slash on every platform so a
        // URI names the same path wherever it is parsed.
        {
            let b = decoded.as_bytes();
            if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
                if let Some(stripped) = decoded.strip_prefix('/') {
                    return Ok(stripped.to_string());
                }
            }
        }
        Ok(decoded)
    } else {
        Ok(format!("//{decoded}"))
    }
}

/// Normalize an inner path (inside an archive or a mem
/// store): leading `/` stripped, `.` and empty components
/// dropped, `..` rejected outright — an inner path must
/// never be able to name something outside its container.
pub fn normalize_inner(s: &str) -> PvResult<String> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in s.split('/') {
        match comp {
            "" | "." => {}
            ".." => return err(ERR_INVALID_ARG, "\"..\" is not allowed in an inner path"),
            c => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

fn valid_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

pub const BUILTIN_SCHEMES: [&str; 4] = ["file", "zip", "tar", "mem"];

/// A parsed URI, dispatched to its backend.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedUri {
    File {
        uri: String,
        path: String,
    },
    Archive {
        /// `"zip"` or `"tar"`.
        kind: String,
        uri: String,
        archive_path: String,
        /// Normalized inner path; `""` is the archive root.
        inner: String,
    },
    Mem {
        uri: String,
        name: String,
        /// Normalized inner path; `""` is the store root.
        inner: String,
    },
    Custom {
        uri: String,
        scheme: String,
    },
}

impl ParsedUri {
    /// The provider name reported in JSON output.
    pub fn provider(&self) -> String {
        match self {
            ParsedUri::File { .. } => "file".to_string(),
            ParsedUri::Archive { kind, .. } => kind.clone(),
            ParsedUri::Mem { .. } => "mem".to_string(),
            ParsedUri::Custom { scheme, .. } => scheme.clone(),
        }
    }
    pub fn uri(&self) -> &str {
        match self {
            ParsedUri::File { uri, .. }
            | ParsedUri::Archive { uri, .. }
            | ParsedUri::Mem { uri, .. }
            | ParsedUri::Custom { uri, .. } => uri,
        }
    }
}

/// Parse a ProviderKit URI (grammar in the header):
/// `<scheme>://<location>`.
pub fn parse_uri(uri: &str) -> PvResult<ParsedUri> {
    if uri.is_empty() {
        return err(ERR_INVALID_ARG, "URI must not be empty");
    }
    if uri.chars().any(|c| (c as u32) < 0x20) {
        return err(ERR_INVALID_ARG, "URI contains a control character");
    }
    let sep = uri.find("://").ok_or((
        ERR_INVALID_ARG,
        "URI must look like <scheme>://<location>".to_string(),
    ))?;
    let scheme = &uri[..sep];
    if !valid_scheme(scheme) {
        return err(
            ERR_INVALID_ARG,
            format!("invalid scheme \"{scheme}\" (lowercase letter, then lowercase letters, digits, '+', '-' or '.')"),
        );
    }
    let rest = &uri[sep + 3..];
    match scheme {
        "file" => Ok(ParsedUri::File {
            uri: uri.to_string(),
            path: location_to_path(rest)?,
        }),
        "zip" | "tar" => {
            // zip:///path/to/archive.zip!/inner/path ; the "!"
            // separator is required only when an inner path
            // is present: zip:///a.zip and zip:///a.zip! both
            // name the archive root.
            let (archive_loc, inner_raw) = match rest.find('!') {
                Some(i) => {
                    let inner = rest[i + 1..].strip_prefix('/').unwrap_or(&rest[i + 1..]);
                    (&rest[..i], inner)
                }
                None => (rest, ""),
            };
            if archive_loc.is_empty() {
                return err(ERR_INVALID_ARG, "archive URI has an empty archive path");
            }
            Ok(ParsedUri::Archive {
                kind: scheme.to_string(),
                uri: uri.to_string(),
                archive_path: location_to_path(archive_loc)?,
                inner: normalize_inner(inner_raw)?,
            })
        }
        "mem" => {
            let (name, inner_raw) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, ""),
            };
            if name.is_empty() {
                return err(
                    ERR_INVALID_ARG,
                    "mem URI must name a store: mem://<name>/...",
                );
            }
            Ok(ParsedUri::Mem {
                uri: uri.to_string(),
                name: name.to_string(),
                inner: normalize_inner(inner_raw)?,
            })
        }
        _ => {
            if is_registered(scheme) {
                Ok(ParsedUri::Custom {
                    uri: uri.to_string(),
                    scheme: scheme.to_string(),
                })
            } else {
                err(
                    ERR_UNKNOWN_PROVIDER,
                    format!("no provider is registered for scheme \"{scheme}\""),
                )
            }
        }
    }
}

/// Join a child name onto a URI for list output.
fn child_uri(base_uri: &str, full_inner_or_name: &str) -> String {
    // Archive URIs: children are addressed from the archive
    // root, so the caller passes the full inner path.
    format!("{base_uri}/{full_inner_or_name}")
}

// ---------------------------------------------------------------------------
// Entries and stats
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    /// Full URI of this entry.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: Option<i64>,
}

impl Entry {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"name\":\"{}\",\"path\":\"{}\",\"is_dir\":{},\"size\":{},\"modified_unix\":{}}}",
            json_escape(&self.name),
            json_escape(&self.path),
            self.is_dir,
            self.size,
            opt_i64(self.modified_unix)
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: Option<i64>,
    pub provider: String,
}

impl Stat {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"exists\":{},\"is_dir\":{},\"size\":{},\"modified_unix\":{},\"provider\":\"{}\"}}",
            self.exists,
            self.is_dir,
            self.size,
            opt_i64(self.modified_unix),
            json_escape(&self.provider)
        )
    }
    fn missing(provider: &str) -> Stat {
        Stat {
            exists: false,
            is_dir: false,
            size: 0,
            modified_unix: None,
            provider: provider.to_string(),
        }
    }
}

fn list_json(entries: &[Entry], provider: &str) -> String {
    let items: Vec<String> = entries.iter().map(|e| e.to_json()).collect();
    format!(
        "{{\"entries\":[{}],\"provider\":\"{}\"}}",
        items.join(","),
        json_escape(provider)
    )
}

/// One flat-store row: (full inner path, is_dir, size,
/// modified). The shared shape of the zip, tar and mem stores.
pub type FlatRow = (String, bool, u64, Option<i64>);
/// A flat store's rows.
pub type FlatRows = Vec<FlatRow>;

/// Split a normalized inner path into (direct children) from a
/// flat list of `(full_inner, is_dir, size, modified)` rows.
/// Shared by the zip, tar and mem backends, whose stores are
/// flat. `prefix` is `""` for the root. Children are sorted by
/// name; directory children are synthesized from deeper rows
/// when no explicit directory row exists.
fn flat_list(rows: &FlatRows, prefix: &str, make_uri: &dyn Fn(&str) -> String) -> Vec<Entry> {
    let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();
    let pfx = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    for (full, is_dir, size, modified) in rows {
        if full.is_empty() || *full == prefix {
            continue;
        }
        let Some(rest) = full.strip_prefix(&pfx) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let (first, deeper) = match rest.find('/') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        let child_full = if pfx.is_empty() {
            first.to_string()
        } else {
            format!("{pfx}{first}")
        };
        if deeper {
            by_name.entry(first.to_string()).or_insert(Entry {
                name: first.to_string(),
                path: make_uri(&child_full),
                is_dir: true,
                size: 0,
                modified_unix: None,
            });
        } else {
            by_name.insert(
                first.to_string(),
                Entry {
                    name: first.to_string(),
                    path: make_uri(&child_full),
                    is_dir: *is_dir,
                    size: if *is_dir { 0 } else { *size },
                    modified_unix: *modified,
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// Stat against a flat row list: exact file/dir rows win, then
/// a synthesized directory when any row lives under `inner`.
fn flat_stat(rows: &[(String, bool, u64, Option<i64>)], inner: &str, provider: &str) -> Stat {
    if inner.is_empty() {
        return Stat {
            exists: true,
            is_dir: true,
            size: 0,
            modified_unix: None,
            provider: provider.to_string(),
        };
    }
    for (full, is_dir, size, modified) in rows {
        if full == inner {
            return Stat {
                exists: true,
                is_dir: *is_dir,
                size: if *is_dir { 0 } else { *size },
                modified_unix: *modified,
                provider: provider.to_string(),
            };
        }
    }
    let pfx = format!("{inner}/");
    if rows.iter().any(|(full, ..)| full.starts_with(&pfx)) {
        return Stat {
            exists: true,
            is_dir: true,
            size: 0,
            modified_unix: None,
            provider: provider.to_string(),
        };
    }
    Stat::missing(provider)
}

// ---------------------------------------------------------------------------
// file:// backend
// ---------------------------------------------------------------------------

fn file_stat(path: &str) -> PvResult<Stat> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Stat {
            exists: true,
            is_dir: m.is_dir(),
            size: if m.is_dir() { 0 } else { m.len() },
            modified_unix: m.modified().ok().and_then(platform::system_time_to_unix),
            provider: "file".to_string(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Stat::missing("file")),
        Err(e) => io_err(&format!("cannot stat \"{path}\""), e),
    }
}

fn file_list(uri: &str, path: &str) -> PvResult<Vec<Entry>> {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return io_err(&format!("cannot list \"{path}\""), e),
    };
    if !md.is_dir() {
        return err(ERR_INVALID_ARG, format!("\"{uri}\" is not a directory"));
    }
    let rd = std::fs::read_dir(path)
        .map_err(|e| (ERR_PLATFORM, format!("cannot list \"{path}\": {e}")))?;
    let base = uri.trim_end_matches('/');
    let mut entries: Vec<Entry> = Vec::new();
    for item in rd {
        let item = item.map_err(|e| (ERR_PLATFORM, format!("cannot list \"{path}\": {e}")))?;
        let name = item.file_name().to_string_lossy().into_owned();
        let ft = item
            .file_type()
            .map_err(|e| (ERR_PLATFORM, format!("cannot inspect \"{name}\": {e}")))?;
        let m = item.metadata().ok();
        entries.push(Entry {
            path: child_uri(base, &name),
            name,
            is_dir: ft.is_dir(),
            size: if ft.is_dir() {
                0
            } else {
                m.as_ref().map(|m| m.len()).unwrap_or(0)
            },
            modified_unix: m
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(platform::system_time_to_unix),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

// ---------------------------------------------------------------------------
// mem:// backend
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MemFile {
    data: Vec<u8>,
    modified_unix: Option<i64>,
}

fn mem_stores() -> &'static Mutex<HashMap<String, BTreeMap<String, MemFile>>> {
    static STORES: OnceLock<Mutex<HashMap<String, BTreeMap<String, MemFile>>>> = OnceLock::new();
    STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn valid_mem_name(name: &str) -> PvResult<()> {
    if name.is_empty()
        || name.contains('/')
        || name.chars().any(|c| (c as u32) < 0x20 || c.is_whitespace())
    {
        return err(
            ERR_INVALID_ARG,
            "mem store name must be non-empty and contain no '/', whitespace or control characters",
        );
    }
    Ok(())
}

/// Add (or replace) a file in a mem store from raw bytes.
/// The rlib-side companion of `pv_mem_add_from_file`.
pub fn mem_add_bytes(name: &str, inner_path: &str, data: &[u8]) -> PvResult<()> {
    valid_mem_name(name)?;
    let inner = normalize_inner(inner_path)?;
    if inner.is_empty() {
        return err(ERR_INVALID_ARG, "mem file path must not be empty");
    }
    let mut stores = lock(mem_stores());
    stores.entry(name.to_string()).or_default().insert(
        inner,
        MemFile {
            data: data.to_vec(),
            modified_unix: None,
        },
    );
    Ok(())
}

/// Read a local file and store its bytes in a mem store
/// (chunked local read; the store itself is in memory, so
/// the bytes end up in memory by definition of the store).
pub fn mem_add_from_file(name: &str, inner_path: &str, local_path: &str) -> PvResult<()> {
    valid_mem_name(name)?;
    let inner = normalize_inner(inner_path)?;
    if inner.is_empty() {
        return err(ERR_INVALID_ARG, "mem file path must not be empty");
    }
    let mut f = std::fs::File::open(local_path)
        .map_err(|e| (e_code(&e), format!("cannot open \"{local_path}\": {e}")))?;
    let modified = f
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(platform::system_time_to_unix);
    let mut data = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| (ERR_PLATFORM, format!("cannot read \"{local_path}\": {e}")))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let mut stores = lock(mem_stores());
    stores.entry(name.to_string()).or_default().insert(
        inner,
        MemFile {
            data,
            modified_unix: modified,
        },
    );
    Ok(())
}

fn e_code(e: &std::io::Error) -> i32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => ERR_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => ERR_ACCESS_DENIED,
        _ => ERR_PLATFORM,
    }
}

fn mem_rows(name: &str) -> PvResult<FlatRows> {
    let stores = lock(mem_stores());
    let store = stores.get(name).ok_or((
        ERR_NOT_FOUND,
        format!("mem store \"{name}\" has no files (add one with pv_mem_add_from_file first)"),
    ))?;
    Ok(store
        .iter()
        .map(|(k, v)| (k.clone(), false, v.data.len() as u64, v.modified_unix))
        .collect())
}

fn mem_get(name: &str, inner: &str) -> PvResult<Vec<u8>> {
    let stores = lock(mem_stores());
    let store = stores
        .get(name)
        .ok_or((ERR_NOT_FOUND, format!("mem store \"{name}\" has no files")))?;
    store.get(inner).map(|f| f.data.clone()).ok_or((
        ERR_NOT_FOUND,
        format!("\"mem://{name}/{inner}\" was not found"),
    ))
}

// ---------------------------------------------------------------------------
// zip:// backend — End Of Central Directory + central
// directory; STORED entries are read fully, DEFLATE entries
// can be listed/stated but not read in v0.1.
// ---------------------------------------------------------------------------

const ZIP_METHOD_STORED: u16 = 0;
const ZIP_METHOD_DEFLATE: u16 = 8;

#[derive(Clone, Debug)]
struct ZipEntry {
    /// Raw entry name as stored (may end in '/' for dirs).
    raw_name: String,
    /// Normalized inner path (no trailing '/').
    name: String,
    is_dir: bool,
    method: u16,
    crc: u32,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
    modified_unix: Option<i64>,
}

fn u16le(b: &[u8], at: usize) -> PvResult<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or((ERR_INTEGRITY, "unexpected end of archive data".to_string()))
}
fn u32le(b: &[u8], at: usize) -> PvResult<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or((ERR_INTEGRITY, "unexpected end of archive data".to_string()))
}

/// MS-DOS date/time (ZIP timestamps) to Unix seconds (UTC).
/// Invalid fields yield None instead of a made-up time.
fn dos_to_unix(date: u16, time: u16) -> Option<i64> {
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as i64;
    let day = (date & 0x1F) as i64;
    let hour = ((time >> 11) & 0x1F) as i64;
    let min = ((time >> 5) & 0x3F) as i64;
    let sec = ((time & 0x1F) as i64) * 2;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

struct ZipArchive {
    entries: Vec<ZipEntry>,
}

impl ZipArchive {
    fn open(path: &str) -> PvResult<ZipArchive> {
        let mut f = std::fs::File::open(path)
            .map_err(|e| (e_code(&e), format!("cannot open archive \"{path}\": {e}")))?;
        let len = f
            .metadata()
            .map_err(|e| {
                (
                    ERR_PLATFORM,
                    format!("cannot inspect archive \"{path}\": {e}"),
                )
            })?
            .len();
        if len < 22 {
            return err(
                ERR_INTEGRITY,
                format!("\"{path}\" is too small to be a ZIP archive (no end-of-central-directory record)"),
            );
        }
        // The EOCD is in the last 64 KiB + 22 bytes (max comment).
        let tail_len = len.min(65557);
        f.seek(SeekFrom::Start(len - tail_len))
            .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
        let mut tail = vec![0u8; tail_len as usize];
        f.read_exact(&mut tail)
            .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
        let mut eocd: Option<usize> = None;
        if tail.len() >= 22 {
            for i in (0..=tail.len() - 22).rev() {
                if tail[i..i + 4] == [0x50, 0x4B, 0x05, 0x06] {
                    let comment = u16le(&tail, i + 20)? as usize;
                    if i + 22 + comment == tail.len() {
                        eocd = Some(i);
                        break;
                    }
                }
            }
        }
        let eocd = eocd.ok_or((
            ERR_INTEGRITY,
            format!("\"{path}\" has no valid end-of-central-directory record (not a ZIP archive, or it is truncated)"),
        ))?;
        let disk = u16le(&tail, eocd + 4)?;
        let cd_disk = u16le(&tail, eocd + 6)?;
        let total = u16le(&tail, eocd + 10)? as u64;
        let cd_size = u32le(&tail, eocd + 12)? as u64;
        let cd_offset = u32le(&tail, eocd + 16)? as u64;
        if disk != 0 || cd_disk != 0 {
            return err(
                ERR_NOT_SUPPORTED,
                "multi-disk ZIP archives are not supported",
            );
        }
        if cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF || total == 0xFFFF {
            return err(
                ERR_NOT_SUPPORTED,
                "ZIP64 archives are not supported in v0.1",
            );
        }
        f.seek(SeekFrom::Start(cd_offset))
            .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
        let mut cd = vec![0u8; cd_size as usize];
        f.read_exact(&mut cd).map_err(|e| {
            (
                ERR_INTEGRITY,
                format!("central directory of \"{path}\" is truncated: {e}"),
            )
        })?;
        let mut entries = Vec::new();
        let mut pos = 0usize;
        for _ in 0..total {
            if cd.get(pos..pos + 4) != Some(&[0x50, 0x4B, 0x01, 0x02]) {
                return err(
                    ERR_INTEGRITY,
                    "central directory entry has a bad signature (archive is corrupt)",
                );
            }
            let method = u16le(&cd, pos + 10)?;
            let mtime = u16le(&cd, pos + 12)?;
            let mdate = u16le(&cd, pos + 14)?;
            let crc = u32le(&cd, pos + 16)?;
            let comp = u32le(&cd, pos + 20)? as u64;
            let uncomp = u32le(&cd, pos + 24)? as u64;
            let nlen = u16le(&cd, pos + 28)? as usize;
            let xlen = u16le(&cd, pos + 30)? as usize;
            let clen = u16le(&cd, pos + 32)? as usize;
            let local_offset = u32le(&cd, pos + 42)? as u64;
            if comp == 0xFFFF_FFFF || uncomp == 0xFFFF_FFFF || local_offset == 0xFFFF_FFFF {
                return err(ERR_NOT_SUPPORTED, "ZIP64 entries are not supported in v0.1");
            }
            let name_bytes = cd.get(pos + 46..pos + 46 + nlen).ok_or((
                ERR_INTEGRITY,
                "central directory name is truncated".to_string(),
            ))?;
            let raw_name = String::from_utf8_lossy(name_bytes).into_owned();
            let is_dir = raw_name.ends_with('/');
            if let Ok(name) = normalize_inner(raw_name.trim_end_matches('/')) {
                entries.push(ZipEntry {
                    raw_name,
                    name,
                    is_dir,
                    method,
                    crc,
                    comp_size: comp,
                    uncomp_size: uncomp,
                    local_offset,
                    modified_unix: dos_to_unix(mdate, mtime),
                });
            }
            // Entries whose names fail normalization (e.g. "..")
            // are skipped: they are unaddressable by design.
            pos += 46 + nlen + xlen + clen;
        }
        Ok(ZipArchive { entries })
    }

    fn rows(&self) -> FlatRows {
        self.entries
            .iter()
            .map(|e| (e.name.clone(), e.is_dir, e.uncomp_size, e.modified_unix))
            .collect()
    }

    fn find(&self, inner: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == inner)
    }

    /// A reader over one STORED entry's data, CRC-checked.
    fn stored_reader(
        &self,
        path: &str,
        entry: &ZipEntry,
    ) -> PvResult<CrcReader<std::io::Take<std::fs::File>>> {
        if entry.method == ZIP_METHOD_DEFLATE {
            return err(
                ERR_NOT_SUPPORTED,
                format!(
                    "\"{}\" is DEFLATE-compressed and DEFLATE decompression is not built into ProviderKit v0.1; listing and stat work, reading does not",
                    entry.raw_name
                ),
            );
        }
        if entry.method != ZIP_METHOD_STORED {
            return err(
                ERR_NOT_SUPPORTED,
                format!(
                    "ZIP entry \"{}\" uses unsupported compression method {}",
                    entry.raw_name, entry.method
                ),
            );
        }
        let mut f = std::fs::File::open(path)
            .map_err(|e| (e_code(&e), format!("cannot open archive \"{path}\": {e}")))?;
        f.seek(SeekFrom::Start(entry.local_offset))
            .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
        let mut hdr = [0u8; 30];
        f.read_exact(&mut hdr).map_err(|e| {
            (
                ERR_INTEGRITY,
                format!("local header of \"{}\" is truncated: {e}", entry.raw_name),
            )
        })?;
        if hdr[0..4] != [0x50, 0x4B, 0x03, 0x04] {
            return err(
                ERR_INTEGRITY,
                format!(
                    "local header of \"{}\" has a bad signature (archive is corrupt)",
                    entry.raw_name
                ),
            );
        }
        let nlen = u16::from_le_bytes([hdr[26], hdr[27]]) as u64;
        let xlen = u16::from_le_bytes([hdr[28], hdr[29]]) as u64;
        f.seek(SeekFrom::Start(entry.local_offset + 30 + nlen + xlen))
            .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
        Ok(CrcReader::new(f.take(entry.comp_size)))
    }
}

// ---------------------------------------------------------------------------
// tar:// backend — ustar headers, plain uncompressed TAR.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct TarEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified_unix: Option<i64>,
    data_offset: u64,
    typeflag: u8,
}

fn parse_octal(field: &[u8]) -> PvResult<u64> {
    if !field.is_empty() && field[0] & 0x80 != 0 {
        return err(
            ERR_NOT_SUPPORTED,
            "base-256 TAR numeric fields are not supported in v0.1",
        );
    }
    let mut text = String::new();
    for &b in field {
        if b == 0 || b == b' ' {
            if !text.is_empty() {
                break;
            }
            continue;
        }
        text.push(b as char);
    }
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(&text, 8).map_err(|_| {
        (
            ERR_INTEGRITY,
            format!("TAR header has an invalid octal field \"{text}\""),
        )
    })
}

struct TarArchive {
    entries: Vec<TarEntry>,
}

impl TarArchive {
    fn open(path: &str) -> PvResult<TarArchive> {
        let mut f = std::fs::File::open(path)
            .map_err(|e| (e_code(&e), format!("cannot open archive \"{path}\": {e}")))?;
        let len = f
            .metadata()
            .map_err(|e| {
                (
                    ERR_PLATFORM,
                    format!("cannot inspect archive \"{path}\": {e}"),
                )
            })?
            .len();
        let mut entries = Vec::new();
        let mut offset: u64 = 0;
        let mut zero_blocks = 0;
        while offset + 512 <= len {
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
            let mut hdr = [0u8; 512];
            f.read_exact(&mut hdr)
                .map_err(|e| (ERR_PLATFORM, format!("cannot read archive \"{path}\": {e}")))?;
            if hdr.iter().all(|&b| b == 0) {
                zero_blocks += 1;
                if zero_blocks >= 2 {
                    break;
                }
                offset += 512;
                continue;
            }
            zero_blocks = 0;
            // Header checksum: sum of all bytes with the
            // checksum field itself counted as spaces.
            let stored_sum = parse_octal(&hdr[148..156])?;
            let mut sum: u64 = 0;
            for (i, &b) in hdr.iter().enumerate() {
                sum += if (148..156).contains(&i) {
                    b' ' as u64
                } else {
                    b as u64
                };
            }
            if sum != stored_sum {
                return err(
                    ERR_INTEGRITY,
                    format!(
                        "TAR header at offset {offset} has a bad checksum (archive is corrupt)"
                    ),
                );
            }
            let name_raw = cstr_field(&hdr[0..100]);
            let prefix_raw = if &hdr[257..262] == b"ustar" {
                cstr_field(&hdr[345..500])
            } else {
                String::new()
            };
            let full_raw = if prefix_raw.is_empty() {
                name_raw
            } else {
                format!("{prefix_raw}/{name_raw}")
            };
            let size = parse_octal(&hdr[124..136])?;
            let mtime = parse_octal(&hdr[136..148])? as i64;
            let typeflag = hdr[156];
            let is_dir = typeflag == b'5' || full_raw.ends_with('/');
            let data_offset = offset + 512;
            if let Ok(name) = normalize_inner(full_raw.trim_end_matches('/')) {
                if !name.is_empty() {
                    entries.push(TarEntry {
                        name,
                        is_dir,
                        size: if is_dir { 0 } else { size },
                        modified_unix: Some(mtime),
                        data_offset,
                        typeflag,
                    });
                }
            }
            offset = data_offset + size.div_ceil(512) * 512;
        }
        Ok(TarArchive { entries })
    }

    fn rows(&self) -> FlatRows {
        self.entries
            .iter()
            .map(|e| (e.name.clone(), e.is_dir, e.size, e.modified_unix))
            .collect()
    }

    fn find(&self, inner: &str) -> Option<&TarEntry> {
        self.entries.iter().find(|e| e.name == inner)
    }
}

fn cstr_field(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn archive_root_uri(kind: &str, archive_path: &str) -> String {
    if let Some(rest) = archive_path.strip_prefix("//") {
        format!("{kind}://{rest}!")
    } else {
        format!("{kind}://{archive_path}!")
    }
}

// ---------------------------------------------------------------------------
// Custom providers (plugin SDK)
// ---------------------------------------------------------------------------

/// Provider list callback. See the header for the exact
/// contract: on PV_OK (0) it stores in `*out_json` a
/// caller-owned (provider-owned) NUL-terminated UTF-8 JSON
/// string shaped like `pv_list` output; ProviderKit copies
/// the string and then releases it by calling the provider's
/// `free_string` callback exactly once. On a nonzero return
/// `*out_json` must be null and the code is propagated.
pub type PvListFn =
    extern "C" fn(user_data: *mut c_void, uri: *const c_char, out_json: *mut *mut c_char) -> i32;
/// Provider stat callback: same string contract; the JSON is
/// `{"exists":bool,"is_dir":bool,"size":uint,"modified_unix":int|null}`
/// (ProviderKit adds the `"provider"` field itself).
pub type PvStatFn =
    extern "C" fn(user_data: *mut c_void, uri: *const c_char, out_json: *mut *mut c_char) -> i32;
/// Provider read callback: writes the URI's full content to
/// the local file at `dest_path` (UTF-8 path), truncating or
/// creating it. Returns PV_OK or a PV_ERR_* code.
pub type PvReadToFileFn =
    extern "C" fn(user_data: *mut c_void, uri: *const c_char, dest_path: *const c_char) -> i32;
/// Releases a string a provider callback returned. Called
/// exactly once per returned string, with the same
/// `user_data` the provider was registered with.
pub type PvFreeStringFn = extern "C" fn(user_data: *mut c_void, p: *mut c_char);

/// The callback table a provider registers. Mirrors
/// `PvProviderCallbacks` in the header exactly (`repr(C)`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProviderCallbacks {
    /// Must be `size_of::<ProviderCallbacks>()` (40 on 64-bit).
    pub struct_size: u32,
    pub list: Option<PvListFn>,
    pub stat: Option<PvStatFn>,
    /// May be null: reads then return PV_ERR_NOT_SUPPORTED.
    pub read_to_file: Option<PvReadToFileFn>,
    /// Required: releases strings returned by list/stat.
    pub free_string: Option<PvFreeStringFn>,
}

#[derive(Clone, Copy)]
struct RegisteredProvider {
    callbacks: ProviderCallbacks,
    /// Stored as a word so the registry stays Send; cast back
    /// to a pointer only at call time.
    user_data: usize,
}

fn providers() -> &'static Mutex<HashMap<String, RegisteredProvider>> {
    static PROVIDERS: OnceLock<Mutex<HashMap<String, RegisteredProvider>>> = OnceLock::new();
    PROVIDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_registered(scheme: &str) -> bool {
    lock(providers()).contains_key(scheme)
}

fn get_provider(scheme: &str) -> Option<RegisteredProvider> {
    lock(providers()).get(scheme).copied()
}

/// Register a custom provider (the Rust side of
/// `pv_register_provider`). `list`, `stat` and `free_string`
/// are required; `read_to_file` may be null.
pub fn register_provider(
    scheme: &str,
    callbacks: &ProviderCallbacks,
    user_data: *mut c_void,
) -> PvResult<()> {
    if !valid_scheme(scheme) {
        return err(ERR_INVALID_ARG, format!("invalid scheme \"{scheme}\""));
    }
    if BUILTIN_SCHEMES.contains(&scheme) {
        return err(
            ERR_DUPLICATE,
            format!("scheme \"{scheme}\" is a built-in provider and cannot be re-registered"),
        );
    }
    if callbacks.struct_size as usize != std::mem::size_of::<ProviderCallbacks>() {
        return err(
            ERR_INVALID_ARG,
            format!(
                "callbacks struct_size is {} but this build expects {}",
                callbacks.struct_size,
                std::mem::size_of::<ProviderCallbacks>()
            ),
        );
    }
    if callbacks.list.is_none() || callbacks.stat.is_none() {
        return err(
            ERR_INVALID_ARG,
            "provider callbacks list and stat must not be null",
        );
    }
    if callbacks.free_string.is_none() {
        return err(
            ERR_INVALID_ARG,
            "provider callbacks free_string must not be null (ProviderKit releases returned strings through it)",
        );
    }
    let mut map = lock(providers());
    if map.contains_key(scheme) {
        return err(
            ERR_DUPLICATE,
            format!("scheme \"{scheme}\" is already registered"),
        );
    }
    map.insert(
        scheme.to_string(),
        RegisteredProvider {
            callbacks: *callbacks,
            user_data: user_data as usize,
        },
    );
    Ok(())
}

/// Remove a custom provider registration. Built-in schemes
/// and unknown schemes are errors.
pub fn unregister_provider(scheme: &str) -> PvResult<()> {
    if BUILTIN_SCHEMES.contains(&scheme) {
        return err(
            ERR_INVALID_ARG,
            format!("built-in provider \"{scheme}\" cannot be unregistered"),
        );
    }
    let mut map = lock(providers());
    if map.remove(scheme).is_some() {
        Ok(())
    } else {
        err(
            ERR_NOT_FOUND,
            format!("no provider is registered for scheme \"{scheme}\""),
        )
    }
}

/// Call a provider string callback, copy its result out, and
/// release the original through the provider's free_string —
/// exactly once, even when the content is unusable.
fn custom_call_string(
    provider: &RegisteredProvider,
    which: &str,
    f: PvListFn,
    uri: &str,
    scheme: &str,
) -> PvResult<String> {
    let uri_c = std::ffi::CString::new(uri)
        .map_err(|_| (ERR_INVALID_ARG, "URI contains a NUL byte".to_string()))?;
    let mut out: *mut c_char = std::ptr::null_mut();
    let user_data = provider.user_data as *mut c_void;
    let rc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f(user_data, uri_c.as_ptr(), &mut out)
    }));
    let rc = match rc {
        Ok(code) => code,
        Err(_) => {
            return err(
                ERR_INTERNAL,
                format!("custom provider \"{scheme}\" panicked in its {which} callback"),
            )
        }
    };
    if out.is_null() {
        if rc == PV_OK {
            return err(
                ERR_INTERNAL,
                format!(
                    "custom provider \"{scheme}\" returned PV_OK from {which} with a null string"
                ),
            );
        }
        return err(
            rc,
            format!("custom provider \"{scheme}\" {which} failed with code {rc}"),
        );
    }
    // Copy first, then free through the provider exactly once.
    let text = unsafe { std::ffi::CStr::from_ptr(out) }
        .to_str()
        .map(|s| s.to_string());
    if let Some(free_fn) = provider.callbacks.free_string {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            free_fn(user_data, out);
        }));
    }
    match text {
        Ok(t) => {
            if rc == PV_OK {
                Ok(t)
            } else {
                err(rc, format!("custom provider \"{scheme}\" {which} failed with code {rc}"))
            }
        }
        Err(_) => err(
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" returned a string from {which} that is not valid UTF-8"),
        ),
    }
}

fn custom_list(provider: &RegisteredProvider, uri: &str, scheme: &str) -> PvResult<String> {
    let f = provider
        .callbacks
        .list
        .ok_or((ERR_INTERNAL, "provider has no list callback".to_string()))?;
    let text = custom_call_string(provider, "list", f, uri, scheme)?;
    let value = json::parse(&text).map_err(|e| {
        (
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" returned invalid JSON from list: {e}"),
        )
    })?;
    match value.get("entries") {
        Some(json::Value::Array(_)) => Ok(text),
        _ => err(
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" list JSON must be an object with an \"entries\" array"),
        ),
    }
}

fn custom_stat(provider: &RegisteredProvider, uri: &str, scheme: &str) -> PvResult<Stat> {
    let f = provider
        .callbacks
        .stat
        .ok_or((ERR_INTERNAL, "provider has no stat callback".to_string()))?;
    let text = custom_call_string(provider, "stat", f, uri, scheme)?;
    let value = json::parse(&text).map_err(|e| {
        (
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" returned invalid JSON from stat: {e}"),
        )
    })?;
    let exists = value.get("exists").and_then(|v| v.as_bool()).ok_or((
        ERR_INTERNAL,
        format!("custom provider \"{scheme}\" stat JSON is missing bool \"exists\""),
    ))?;
    let is_dir = value.get("is_dir").and_then(|v| v.as_bool()).ok_or((
        ERR_INTERNAL,
        format!("custom provider \"{scheme}\" stat JSON is missing bool \"is_dir\""),
    ))?;
    let size = value
        .get("size")
        .and_then(|v| v.as_i64())
        .filter(|n| *n >= 0)
        .ok_or((
            ERR_INTERNAL,
            format!(
                "custom provider \"{scheme}\" stat JSON is missing non-negative integer \"size\""
            ),
        ))? as u64;
    let modified_unix = match value.get("modified_unix") {
        Some(json::Value::Null) | None => None,
        Some(v) => Some(v.as_i64().ok_or((
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" stat JSON \"modified_unix\" must be an integer or null"),
        ))?),
    };
    Ok(Stat {
        exists,
        is_dir,
        size: if is_dir { 0 } else { size },
        modified_unix,
        provider: scheme.to_string(),
    })
}

fn custom_read_to_file(
    provider: &RegisteredProvider,
    uri: &str,
    scheme: &str,
    dest: &str,
) -> PvResult<()> {
    let f = match provider.callbacks.read_to_file {
        Some(f) => f,
        None => {
            return err(
                ERR_NOT_SUPPORTED,
                format!(
                "custom provider \"{scheme}\" does not support reading (no read_to_file callback)"
            ),
            )
        }
    };
    let uri_c = std::ffi::CString::new(uri)
        .map_err(|_| (ERR_INVALID_ARG, "URI contains a NUL byte".to_string()))?;
    let dest_c = std::ffi::CString::new(dest).map_err(|_| {
        (
            ERR_INVALID_ARG,
            "destination path contains a NUL byte".to_string(),
        )
    })?;
    let user_data = provider.user_data as *mut c_void;
    let rc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f(user_data, uri_c.as_ptr(), dest_c.as_ptr())
    }));
    match rc {
        Ok(PV_OK) => Ok(()),
        Ok(code) => err(
            code,
            format!("custom provider \"{scheme}\" read_to_file failed with code {code}"),
        ),
        Err(_) => err(
            ERR_INTERNAL,
            format!("custom provider \"{scheme}\" panicked in its read_to_file callback"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// `pv_stat`: stat one URI. A missing target is not an
/// error — the result has `"exists":false`. Errors are for
/// unusable input (unknown scheme, malformed URI, a missing
/// archive file, a corrupt archive).
pub fn stat(uri: &str) -> PvResult<Stat> {
    let parsed = parse_uri(uri)?;
    match &parsed {
        ParsedUri::File { path, .. } => file_stat(path),
        ParsedUri::Archive {
            kind,
            archive_path,
            inner,
            ..
        } => {
            if kind == "zip" {
                let z = ZipArchive::open(archive_path)?;
                Ok(flat_stat(&z.rows(), inner, "zip"))
            } else {
                let t = TarArchive::open(archive_path)?;
                Ok(flat_stat(&t.rows(), inner, "tar"))
            }
        }
        ParsedUri::Mem { name, inner, .. } => {
            // A store that does not exist yet stats as missing,
            // not as an error (files can still be added later).
            match mem_rows(name) {
                Ok(rows) => Ok(flat_stat(&rows, inner, "mem")),
                Err((ERR_NOT_FOUND, _)) => Ok(Stat::missing("mem")),
                Err(e) => Err(e),
            }
        }
        ParsedUri::Custom { scheme, .. } => {
            let p = get_provider(scheme).ok_or((
                ERR_UNKNOWN_PROVIDER,
                format!("no provider is registered for scheme \"{scheme}\""),
            ))?;
            custom_stat(&p, uri, scheme)
        }
    }
}

/// `pv_list`: list the direct children of a directory URI.
pub fn list(uri: &str) -> PvResult<String> {
    let parsed = parse_uri(uri)?;
    let provider = parsed.provider();
    match &parsed {
        ParsedUri::File { path, .. } => {
            let entries = file_list(uri, path)?;
            Ok(list_json(&entries, &provider))
        }
        ParsedUri::Archive {
            kind,
            archive_path,
            inner,
            ..
        } => {
            let root = archive_root_uri(kind, archive_path);
            let make = |full: &str| format!("{root}/{full}");
            if kind == "zip" {
                let z = ZipArchive::open(archive_path)?;
                let st = flat_stat(&z.rows(), inner, "zip");
                if !st.exists {
                    return err(
                        ERR_NOT_FOUND,
                        format!("\"{uri}\" was not found in the archive"),
                    );
                }
                if !st.is_dir {
                    return err(ERR_INVALID_ARG, format!("\"{uri}\" is not a directory"));
                }
                Ok(list_json(&flat_list(&z.rows(), inner, &make), &provider))
            } else {
                let t = TarArchive::open(archive_path)?;
                let st = flat_stat(&t.rows(), inner, "tar");
                if !st.exists {
                    return err(
                        ERR_NOT_FOUND,
                        format!("\"{uri}\" was not found in the archive"),
                    );
                }
                if !st.is_dir {
                    return err(ERR_INVALID_ARG, format!("\"{uri}\" is not a directory"));
                }
                Ok(list_json(&flat_list(&t.rows(), inner, &make), &provider))
            }
        }
        ParsedUri::Mem { name, inner, .. } => {
            let rows = mem_rows(name)?;
            let st = flat_stat(&rows, inner, "mem");
            if !st.exists {
                return err(ERR_NOT_FOUND, format!("\"{uri}\" was not found"));
            }
            if !st.is_dir {
                return err(ERR_INVALID_ARG, format!("\"{uri}\" is not a directory"));
            }
            let root = format!("mem://{name}");
            let make = |full: &str| format!("{root}/{full}");
            Ok(list_json(&flat_list(&rows, inner, &make), &provider))
        }
        ParsedUri::Custom { scheme, .. } => {
            let p = get_provider(scheme).ok_or((
                ERR_UNKNOWN_PROVIDER,
                format!("no provider is registered for scheme \"{scheme}\""),
            ))?;
            custom_list(&p, uri, scheme)
        }
    }
}

const CHUNK: usize = 65536;

/// Stream `reader` into a new/truncated file at `dest`.
fn stream_to_file<R: Read>(mut reader: R, dest: &str, what: &str) -> PvResult<u64> {
    let mut out = std::fs::File::create(dest)
        .map_err(|e| (e_code(&e), format!("cannot create \"{dest}\": {e}")))?;
    let mut buf = [0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| (ERR_PLATFORM, format!("cannot read {what}: {e}")))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| (e_code(&e), format!("cannot write \"{dest}\": {e}")))?;
        total += n as u64;
    }
    Ok(total)
}

/// Unique scratch path for routing custom-provider reads.
fn temp_path(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!("providerkit-{tag}-{}-{n}.tmp", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// `pv_read_to_file`: copy a URI's content to a local file,
/// chunked — built-in file/zip/tar content is never held
/// whole in memory. mem content is in memory by definition.
pub fn read_to_file(uri: &str, dest_local_path: &str) -> PvResult<()> {
    if dest_local_path.is_empty() {
        return err(ERR_INVALID_ARG, "destination path must not be empty");
    }
    let parsed = parse_uri(uri)?;
    match &parsed {
        ParsedUri::File { path, .. } => {
            let st = file_stat(path)?;
            if !st.exists {
                return err(ERR_NOT_FOUND, format!("\"{uri}\" was not found"));
            }
            if st.is_dir {
                return err(ERR_INVALID_ARG, format!("\"{uri}\" is a directory"));
            }
            if path == dest_local_path {
                return err(ERR_INVALID_ARG, "source and destination are the same file");
            }
            let f = std::fs::File::open(path)
                .map_err(|e| (e_code(&e), format!("cannot open \"{path}\": {e}")))?;
            stream_to_file(f, dest_local_path, &format!("\"{path}\""))?;
            Ok(())
        }
        ParsedUri::Archive {
            kind,
            archive_path,
            inner,
            ..
        } => {
            if inner.is_empty() {
                return err(
                    ERR_INVALID_ARG,
                    format!("\"{uri}\" is the archive root (a directory)"),
                );
            }
            if kind == "zip" {
                let z = ZipArchive::open(archive_path)?;
                let entry = z.find(inner).ok_or((
                    ERR_NOT_FOUND,
                    format!("\"{uri}\" was not found in the archive"),
                ))?;
                if entry.is_dir {
                    return err(ERR_INVALID_ARG, format!("\"{uri}\" is a directory"));
                }
                let entry = entry.clone();
                let mut reader = z.stored_reader(archive_path, &entry)?;
                let written = stream_to_file(&mut reader, dest_local_path, &format!("\"{uri}\""))?;
                if reader.finish() != entry.crc || written != entry.uncomp_size {
                    let _ = std::fs::remove_file(dest_local_path);
                    return err(
                        ERR_INTEGRITY,
                        format!("CRC-32 check failed for \"{}\" (the archive data is corrupt); the partial output file was removed", entry.raw_name),
                    );
                }
                Ok(())
            } else {
                let t = TarArchive::open(archive_path)?;
                let entry = t.find(inner).ok_or((
                    ERR_NOT_FOUND,
                    format!("\"{uri}\" was not found in the archive"),
                ))?;
                if entry.is_dir {
                    return err(ERR_INVALID_ARG, format!("\"{uri}\" is a directory"));
                }
                if !(entry.typeflag == b'0' || entry.typeflag == 0) {
                    return err(
                        ERR_NOT_SUPPORTED,
                        format!("TAR entry \"{inner}\" has typeflag {} and cannot be read as a regular file", entry.typeflag as char),
                    );
                }
                let (off, size) = (entry.data_offset, entry.size);
                let mut f = std::fs::File::open(archive_path).map_err(|e| {
                    (
                        e_code(&e),
                        format!("cannot open archive \"{archive_path}\": {e}"),
                    )
                })?;
                f.seek(SeekFrom::Start(off)).map_err(|e| {
                    (
                        ERR_PLATFORM,
                        format!("cannot read archive \"{archive_path}\": {e}"),
                    )
                })?;
                stream_to_file(f.take(size), dest_local_path, &format!("\"{uri}\""))?;
                Ok(())
            }
        }
        ParsedUri::Mem { name, inner, .. } => {
            let data = mem_get(name, inner)?;
            let mut out = std::fs::File::create(dest_local_path).map_err(|e| {
                (
                    e_code(&e),
                    format!("cannot create \"{dest_local_path}\": {e}"),
                )
            })?;
            out.write_all(&data).map_err(|e| {
                (
                    e_code(&e),
                    format!("cannot write \"{dest_local_path}\": {e}"),
                )
            })?;
            Ok(())
        }
        ParsedUri::Custom { scheme, .. } => {
            let p = get_provider(scheme).ok_or((
                ERR_UNKNOWN_PROVIDER,
                format!("no provider is registered for scheme \"{scheme}\""),
            ))?;
            custom_read_to_file(&p, uri, scheme, dest_local_path)
        }
    }
}

/// Read a URI's whole content into memory, in chunks. Only
/// used where the destination itself is memory (read_text
/// with its cap, or a copy into a mem store).
fn read_all_bytes(uri: &str, parsed: &ParsedUri) -> PvResult<Vec<u8>> {
    match parsed {
        ParsedUri::Mem { name, inner, .. } => mem_get(name, inner),
        ParsedUri::Custom { .. } => {
            let tmp = temp_path("read");
            let r = read_to_file(uri, &tmp);
            let bytes = match r {
                Ok(()) => std::fs::read(&tmp).map_err(|e| {
                    (
                        ERR_PLATFORM,
                        format!("cannot read scratch file \"{tmp}\": {e}"),
                    )
                }),
                Err(e) => Err(e),
            };
            let _ = std::fs::remove_file(&tmp);
            bytes
        }
        _ => {
            let tmp = temp_path("read");
            let r = read_to_file(uri, &tmp);
            let bytes = match r {
                Ok(()) => {
                    // Stream the scratch copy into memory in
                    // chunks rather than one giant read call.
                    let mut f = std::fs::File::open(&tmp).map_err(|e| {
                        (
                            ERR_PLATFORM,
                            format!("cannot read scratch file \"{tmp}\": {e}"),
                        )
                    })?;
                    let mut data = Vec::new();
                    let mut buf = [0u8; CHUNK];
                    loop {
                        let n = f.read(&mut buf).map_err(|e| {
                            (
                                ERR_PLATFORM,
                                format!("cannot read scratch file \"{tmp}\": {e}"),
                            )
                        })?;
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..n]);
                    }
                    Ok(data)
                }
                Err(e) => Err(e),
            };
            let _ = std::fs::remove_file(&tmp);
            bytes
        }
    }
}

/// `pv_read_text`: content of a small file as UTF-8 text.
/// Content larger than `max_bytes` is PV_ERR_TOO_LARGE;
/// content that is not valid UTF-8 is PV_ERR_PARSE.
pub fn read_text(uri: &str, max_bytes: u64) -> PvResult<String> {
    let st = stat(uri)?;
    if !st.exists {
        return err(ERR_NOT_FOUND, format!("\"{uri}\" was not found"));
    }
    if st.is_dir {
        return err(ERR_INVALID_ARG, format!("\"{uri}\" is a directory"));
    }
    if st.size > max_bytes {
        return err(
            ERR_TOO_LARGE,
            format!(
                "\"{uri}\" is {} bytes, larger than the {}-byte cap",
                st.size, max_bytes
            ),
        );
    }
    let parsed = parse_uri(uri)?;
    let bytes = read_all_bytes(uri, &parsed)?;
    if bytes.len() as u64 > max_bytes {
        return err(
            ERR_TOO_LARGE,
            format!("\"{uri}\" is larger than the {max_bytes}-byte cap"),
        );
    }
    String::from_utf8(bytes).map_err(|_| (ERR_PARSE, format!("\"{uri}\" is not valid UTF-8 text")))
}

/// `pv_copy`: copy one file between providers. Writable
/// destinations in v0.1 are `file://` and `mem://` only;
/// zip/tar are read-only and custom providers are read-only
/// as destinations — those return PV_ERR_NOT_SUPPORTED.
pub fn copy(src_uri: &str, dst_uri: &str) -> PvResult<()> {
    let src = parse_uri(src_uri)?;
    let dst = parse_uri(dst_uri)?;
    // Destination writability is checked first so a copy to a
    // read-only provider fails with NOT_SUPPORTED even when
    // the source also has a problem.
    match &dst {
        ParsedUri::File { .. } | ParsedUri::Mem { .. } => {}
        ParsedUri::Archive { kind, .. } => {
            return err(
                ERR_NOT_SUPPORTED,
                format!("the {kind} provider is read-only in v0.1; copying into an archive is not supported"),
            )
        }
        ParsedUri::Custom { scheme, .. } => {
            return err(
                ERR_NOT_SUPPORTED,
                format!("custom provider \"{scheme}\" is read-only as a destination in v0.1"),
            )
        }
    }
    let st = stat(src_uri)?;
    if !st.exists {
        return err(ERR_NOT_FOUND, format!("\"{src_uri}\" was not found"));
    }
    if st.is_dir {
        return err(
            ERR_NOT_SUPPORTED,
            "copying a directory is not supported in v0.1 (copy its files one by one)",
        );
    }
    match &dst {
        ParsedUri::File { path, .. } => read_to_file(src_uri, path),
        ParsedUri::Mem { name, inner, .. } => {
            if inner.is_empty() {
                return err(
                    ERR_INVALID_ARG,
                    "mem destination must name a file: mem://<name>/<path>",
                );
            }
            let bytes = read_all_bytes(src_uri, &src)?;
            mem_add_bytes(name, inner, &bytes)
        }
        _ => unreachable!("destination writability checked above"),
    }
}

/// `pv_providers`: every registered provider + capabilities.
pub fn providers_json() -> String {
    let mut items: Vec<String> = vec![
        "{\"scheme\":\"file\",\"provider\":\"file\",\"read\":true,\"write\":true,\"list\":true}"
            .to_string(),
        "{\"scheme\":\"mem\",\"provider\":\"mem\",\"read\":true,\"write\":true,\"list\":true}"
            .to_string(),
        "{\"scheme\":\"tar\",\"provider\":\"tar\",\"read\":true,\"write\":false,\"list\":true}"
            .to_string(),
        "{\"scheme\":\"zip\",\"provider\":\"zip\",\"read\":true,\"write\":false,\"list\":true}"
            .to_string(),
    ];
    let mut customs: Vec<(String, RegisteredProvider)> = lock(providers())
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    customs.sort_by(|a, b| a.0.cmp(&b.0));
    for (scheme, p) in customs {
        items.push(format!(
            "{{\"scheme\":\"{}\",\"provider\":\"{}\",\"read\":{},\"write\":false,\"list\":true}}",
            json_escape(&scheme),
            json_escape(&scheme),
            p.callbacks.read_to_file.is_some()
        ));
    }
    format!("{{\"providers\":[{}]}}", items.join(","))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir()
                .join(format!("providerkit-test-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn join(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file_uri(local: &str) -> String {
        format!("file://{local}")
    }

    #[test]
    fn file_uri_windows_drive_forms() {
        // file://C:\a\b and file:///C:/a/b are drive-absolute
        // local paths, never UNC hosts.
        match parse_uri("file://C:\\a\\b.txt").unwrap() {
            ParsedUri::File { path, .. } => assert_eq!(path, "C:\\a\\b.txt"),
            other => panic!("expected file URI, got {other:?}"),
        }
        match parse_uri("file:///C:/a/b.txt").unwrap() {
            ParsedUri::File { path, .. } => assert_eq!(path, "C:/a/b.txt"),
            other => panic!("expected file URI, got {other:?}"),
        }
        // A bare name is still a UNC host.
        match parse_uri("file://server/share/f.txt").unwrap() {
            ParsedUri::File { path, .. } => assert_eq!(path, "//server/share/f.txt"),
            other => panic!("expected file URI, got {other:?}"),
        }
    }

    #[test]
    fn escapes_and_crc() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(dos_to_unix(33, 0), Some(315_532_800)); // 1980-01-01 00:00 UTC
    }

    #[test]
    fn uri_grammar() {
        match parse_uri("file:///tmp/a%20b.txt").unwrap() {
            ParsedUri::File { path, .. } => assert_eq!(path, "/tmp/a b.txt"),
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_uri("file://server/share/f.txt").unwrap() {
            ParsedUri::File { path, .. } => assert_eq!(path, "//server/share/f.txt"),
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_uri("zip:///tmp/a.zip!/dir/f.txt").unwrap() {
            ParsedUri::Archive {
                kind,
                archive_path,
                inner,
                ..
            } => {
                assert_eq!(kind, "zip");
                assert_eq!(archive_path, "/tmp/a.zip");
                assert_eq!(inner, "dir/f.txt");
            }
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_uri("tar:///tmp/a.tar!").unwrap() {
            ParsedUri::Archive { inner, .. } => assert_eq!(inner, ""),
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_uri("mem://scratch/dir/f.txt").unwrap() {
            ParsedUri::Mem { name, inner, .. } => {
                assert_eq!(name, "scratch");
                assert_eq!(inner, "dir/f.txt");
            }
            other => panic!("wrong parse: {other:?}"),
        }
        assert_eq!(parse_uri("noscheme").unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(parse_uri("File:///x").unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(parse_uri("wat://x").unwrap_err().0, ERR_UNKNOWN_PROVIDER);
        assert_eq!(
            parse_uri("zip:///a.zip!/../evil").unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(parse_uri("mem:///no-name").unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(parse_uri("").unwrap_err().0, ERR_INVALID_ARG);
    }

    // -- File backend -------------------------------------------------------

    fn make_tree(tag: &str) -> (TempDir, String) {
        let td = TempDir::new(tag);
        std::fs::create_dir_all(td.join("sub")).unwrap();
        std::fs::write(td.join("a.txt"), b"alpha").unwrap();
        std::fs::write(td.join("sub/b.txt"), b"beta-content").unwrap();
        let p = td.path();
        (td, p)
    }

    #[test]
    fn file_list_stat_read() {
        let (_td, root) = make_tree("file");
        let root_uri = file_uri(&root);
        let v = json::parse(&list(&root_uri).unwrap()).unwrap();
        assert_eq!(v.get("provider").unwrap().as_str(), Some("file"));
        let entries = v.get("entries").unwrap().as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].get("name").unwrap().as_str(), Some("a.txt"));
        assert_eq!(entries[0].get("is_dir").unwrap().as_bool(), Some(false));
        assert_eq!(entries[0].get("size").unwrap().as_i64(), Some(5));
        assert!(entries[0].get("modified_unix").unwrap().as_i64().is_some());
        assert_eq!(entries[1].get("name").unwrap().as_str(), Some("sub"));
        assert_eq!(entries[1].get("is_dir").unwrap().as_bool(), Some(true));

        let st = stat(&format!("{root_uri}/sub/b.txt")).unwrap();
        assert!(st.exists && !st.is_dir && st.size == 12);
        let missing = stat(&format!("{root_uri}/nope.txt")).unwrap();
        assert!(!missing.exists);

        assert_eq!(
            read_text(&format!("{root_uri}/a.txt"), 100).unwrap(),
            "alpha"
        );
        assert_eq!(
            read_text(&format!("{root_uri}/a.txt"), 4).unwrap_err().0,
            ERR_TOO_LARGE
        );
        // List on a file is an error, not an empty list.
        assert_eq!(
            list(&format!("{root_uri}/a.txt")).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            list(&format!("{root_uri}/missing-dir")).unwrap_err().0,
            ERR_NOT_FOUND
        );
    }

    #[test]
    fn file_read_to_file_and_copy() {
        let (td, root) = make_tree("copy");
        let dest = td.join("out.txt");
        read_to_file(&format!("{}/sub/b.txt", file_uri(&root)), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"beta-content");
        // Same-file copy is refused (it would truncate its own source).
        let a = td.join("a.txt");
        assert_eq!(
            copy(&file_uri(&a), &file_uri(&a)).unwrap_err().0,
            ERR_INVALID_ARG
        );
        // Directory copy is honestly not supported.
        assert_eq!(
            copy(&file_uri(&root), &file_uri(&td.join("elsewhere")))
                .unwrap_err()
                .0,
            ERR_NOT_SUPPORTED
        );
    }

    // -- ZIP fixtures --------------------------------------------------------

    struct ZipSpec {
        name: &'static str,
        method: u16,
        data: Vec<u8>,
        crc: u32,
        uncomp: u64,
    }
    fn stored(name: &'static str, data: &[u8]) -> ZipSpec {
        ZipSpec {
            name,
            method: 0,
            data: data.to_vec(),
            crc: crc32(data),
            uncomp: data.len() as u64,
        }
    }
    fn build_zip(specs: &[ZipSpec]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for s in specs {
            let offset = out.len() as u32;
            let name = s.name.as_bytes();
            out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&s.method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&33u16.to_le_bytes()); // date 1980-01-01
            out.extend_from_slice(&s.crc.to_le_bytes());
            out.extend_from_slice(&(s.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(s.uncomp as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&s.data);
            central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&s.method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&33u16.to_le_bytes());
            central.extend_from_slice(&s.crc.to_le_bytes());
            central.extend_from_slice(&(s.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(s.uncomp as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(specs.len() as u16).to_le_bytes());
        out.extend_from_slice(&(specs.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn make_zip(tag: &str) -> (TempDir, String) {
        let td = TempDir::new(tag);
        let specs = vec![
            stored("hello.txt", b"hello zip world"),
            stored("dir/inner.txt", b"nested!"),
            ZipSpec {
                name: "packed.bin",
                method: 8, // DEFLATE header; bytes are not a real stream
                data: vec![0x00, 0x01, 0x02],
                crc: 0,
                uncomp: 5,
            },
        ];
        let zp = td.join("test.zip");
        std::fs::write(&zp, build_zip(&specs)).unwrap();
        (td, zp)
    }

    #[test]
    fn zip_list_stat_read_stored() {
        let (_td, zp) = make_zip("zip");
        let root = format!("zip://{zp}!");
        let v = json::parse(&list(&root).unwrap()).unwrap();
        assert_eq!(v.get("provider").unwrap().as_str(), Some("zip"));
        let entries = v.get("entries").unwrap().as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e.get("name").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["dir", "hello.txt", "packed.bin"]);
        let hello = &entries[1];
        assert_eq!(hello.get("size").unwrap().as_i64(), Some(15));
        assert_eq!(
            hello.get("modified_unix").unwrap().as_i64(),
            Some(315_532_800)
        );
        assert_eq!(
            hello.get("path").unwrap().as_str(),
            Some(format!("zip://{zp}!/hello.txt").as_str())
        );

        let dir_list = json::parse(&list(&format!("zip://{zp}!/dir")).unwrap()).unwrap();
        let d = dir_list.get("entries").unwrap().as_array().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].get("name").unwrap().as_str(), Some("inner.txt"));

        let st = stat(&format!("zip://{zp}!/dir")).unwrap();
        assert!(st.exists && st.is_dir);
        assert!(!stat(&format!("zip://{zp}!/absent")).unwrap().exists);

        assert_eq!(
            read_text(&format!("zip://{zp}!/hello.txt"), 1000).unwrap(),
            "hello zip world"
        );
        assert_eq!(
            read_text(&format!("zip://{zp}!/dir/inner.txt"), 1000).unwrap(),
            "nested!"
        );
    }

    #[test]
    fn zip_deflate_lists_but_read_is_not_supported() {
        let (_td, zp) = make_zip("zipdef");
        let uri = format!("zip://{zp}!/packed.bin");
        let st = stat(&uri).unwrap();
        assert!(st.exists && st.size == 5);
        let td2 = TempDir::new("zipdefout");
        let dest = td2.join("packed.bin");
        let e = read_to_file(&uri, &dest).unwrap_err();
        assert_eq!(e.0, ERR_NOT_SUPPORTED);
        assert!(e.1.contains("DEFLATE"), "message was: {}", e.1);
        assert_eq!(read_text(&uri, 100).unwrap_err().0, ERR_NOT_SUPPORTED);
    }

    #[test]
    fn zip_crc_mismatch_is_integrity_error_and_removes_output() {
        let td = TempDir::new("zipcrc");
        // Stored bytes "hello" but the CRC recorded is for "hellp".
        let bad = ZipSpec {
            name: "bad.txt",
            method: 0,
            data: b"hello".to_vec(),
            crc: crc32(b"hellp"),
            uncomp: 5,
        };
        let zp = td.join("bad.zip");
        std::fs::write(&zp, build_zip(&[bad])).unwrap();
        let dest = td.join("out.txt");
        let e = read_to_file(&format!("zip://{zp}!/bad.txt"), &dest).unwrap_err();
        assert_eq!(e.0, ERR_INTEGRITY);
        assert!(
            !PathBuf::from(&dest).exists(),
            "partial output must be removed"
        );
    }

    #[test]
    fn zip_missing_and_corrupt_archive_errors() {
        assert_eq!(
            list("zip:///definitely/not/here.zip!").unwrap_err().0,
            ERR_NOT_FOUND
        );
        let td = TempDir::new("zipbad");
        let zp = td.join("junk.zip");
        std::fs::write(&zp, b"this is not a zip at all, just text padding...").unwrap();
        assert_eq!(list(&format!("zip://{zp}!")).unwrap_err().0, ERR_INTEGRITY);
    }

    // -- TAR fixtures --------------------------------------------------------

    fn tar_header(name: &str, size: u64, typeflag: u8, mtime: u64) -> [u8; 512] {
        let mut h = [0u8; 512];
        let nb = name.as_bytes();
        h[..nb.len()].copy_from_slice(nb);
        let put = |h: &mut [u8; 512], at: usize, len: usize, val: &str| {
            h[at..at + val.len()].copy_from_slice(val.as_bytes());
            let _ = len;
        };
        put(&mut h, 100, 8, "0000644\0");
        put(&mut h, 108, 8, "0000000\0");
        put(&mut h, 116, 8, "0000000\0");
        put(&mut h, 124, 12, &format!("{size:011o}\0"));
        put(&mut h, 136, 12, &format!("{mtime:011o}\0"));
        for b in &mut h[148..156] {
            *b = b' ';
        }
        h[156] = typeflag;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u64 = h.iter().map(|&b| b as u64).sum();
        put(&mut h, 148, 8, &format!("{sum:06o}\0 "));
        h
    }

    fn build_tar() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&tar_header("readme.txt", 9, b'0', 1_700_000_000));
        out.extend_from_slice(b"tar hello");
        out.resize(out.len() + (512 - 9), 0);
        out.extend_from_slice(&tar_header("docs/", 0, b'5', 1_700_000_000));
        out.extend_from_slice(&tar_header("docs/guide.txt", 5, b'0', 1_700_000_100));
        out.extend_from_slice(b"guide");
        out.resize(out.len() + (512 - 5), 0);
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    #[test]
    fn tar_list_stat_read() {
        let td = TempDir::new("tar");
        let tp = td.join("test.tar");
        std::fs::write(&tp, build_tar()).unwrap();
        let root = format!("tar://{tp}!");
        let v = json::parse(&list(&root).unwrap()).unwrap();
        assert_eq!(v.get("provider").unwrap().as_str(), Some("tar"));
        let entries = v.get("entries").unwrap().as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e.get("name").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["docs", "readme.txt"]);
        let st = stat(&format!("tar://{tp}!/docs/guide.txt")).unwrap();
        assert!(st.exists && st.size == 5 && st.modified_unix == Some(1_700_000_100));
        assert_eq!(
            read_text(&format!("tar://{tp}!/readme.txt"), 100).unwrap(),
            "tar hello"
        );
        let dest = td.join("guide-out.txt");
        read_to_file(&format!("tar://{tp}!/docs/guide.txt"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"guide");
        // Corrupt one name byte -> the header checksum must fail.
        let mut bytes = build_tar();
        bytes[3] ^= 0x01;
        let cp = td.join("corrupt.tar");
        std::fs::write(&cp, bytes).unwrap();
        assert_eq!(list(&format!("tar://{cp}!")).unwrap_err().0, ERR_INTEGRITY);
    }

    // -- mem + cross-provider copy ------------------------------------------

    #[test]
    fn mem_and_cross_copy_round_trip() {
        let (td, root) = make_tree("memrt");
        let src_uri = format!("{}/sub/b.txt", file_uri(&root));
        copy(&src_uri, "mem://rtstore/sub/b.txt").unwrap();
        let st = stat("mem://rtstore/sub/b.txt").unwrap();
        assert!(st.exists && st.size == 12 && st.provider == "mem");
        assert_eq!(
            read_text("mem://rtstore/sub/b.txt", 100).unwrap(),
            "beta-content"
        );
        let back = td.join("back.txt");
        copy("mem://rtstore/sub/b.txt", &file_uri(&back)).unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), b"beta-content");

        // mem_add_from_file (the FFI operation's core).
        mem_add_from_file("rtstore", "direct.txt", &td.join("a.txt")).unwrap();
        assert_eq!(read_text("mem://rtstore/direct.txt", 100).unwrap(), "alpha");
        let v = json::parse(&list("mem://rtstore").unwrap()).unwrap();
        let entries = v.get("entries").unwrap().as_array().unwrap();
        assert_eq!(entries.len(), 2); // direct.txt + sub/

        // zip -> mem -> file, and file -> zip refused honestly.
        let (_ztd, zp) = make_zip("zipx");
        copy(
            &format!("zip://{zp}!/hello.txt"),
            "mem://rtstore/fromzip.txt",
        )
        .unwrap();
        assert_eq!(
            read_text("mem://rtstore/fromzip.txt", 100).unwrap(),
            "hello zip world"
        );
        assert_eq!(
            copy(&src_uri, &format!("zip://{zp}!/new.txt"))
                .unwrap_err()
                .0,
            ERR_NOT_SUPPORTED
        );
        assert_eq!(
            copy(&src_uri, &format!("tar://{zp}!/new.txt"))
                .unwrap_err()
                .0,
            ERR_NOT_SUPPORTED
        );
        // mem store that does not exist: stat says missing, list errors.
        assert!(!stat("mem://never-made/x.txt").unwrap().exists);
        assert_eq!(list("mem://never-made").unwrap_err().0, ERR_NOT_FOUND);
    }

    #[test]
    fn providers_json_lists_builtins() {
        let v = json::parse(&providers_json()).unwrap();
        let ps = v.get("providers").unwrap().as_array().unwrap();
        let schemes: Vec<&str> = ps
            .iter()
            .map(|p| p.get("scheme").unwrap().as_str().unwrap())
            .collect();
        assert!(schemes.contains(&"file") && schemes.contains(&"zip"));
        assert!(schemes.contains(&"tar") && schemes.contains(&"mem"));
        for p in ps {
            assert!(p.get("read").is_some() && p.get("write").is_some() && p.get("list").is_some());
        }
        let zip = ps
            .iter()
            .find(|p| p.get("scheme").unwrap().as_str() == Some("zip"))
            .unwrap();
        assert_eq!(zip.get("write").unwrap().as_bool(), Some(false));
    }

    // -- Custom provider ------------------------------------------------------

    extern "C" fn demo_list(_ud: *mut c_void, _uri: *const c_char, out: *mut *mut c_char) -> i32 {
        let s = std::ffi::CString::new(
            "{\"entries\":[{\"name\":\"one.txt\",\"path\":\"demokit://one.txt\",\"is_dir\":false,\"size\":3,\"modified_unix\":null}],\"provider\":\"demokit\"}",
        )
        .unwrap();
        unsafe { *out = s.into_raw() };
        PV_OK
    }
    extern "C" fn demo_stat(_ud: *mut c_void, _uri: *const c_char, out: *mut *mut c_char) -> i32 {
        let s = std::ffi::CString::new(
            "{\"exists\":true,\"is_dir\":false,\"size\":3,\"modified_unix\":42}",
        )
        .unwrap();
        unsafe { *out = s.into_raw() };
        PV_OK
    }
    extern "C" fn demo_read(_ud: *mut c_void, _uri: *const c_char, dest: *const c_char) -> i32 {
        let dest = unsafe { std::ffi::CStr::from_ptr(dest) }.to_str().unwrap();
        match std::fs::write(dest, b"one") {
            Ok(()) => PV_OK,
            Err(_) => ERR_PLATFORM,
        }
    }
    extern "C" fn demo_free(_ud: *mut c_void, p: *mut c_char) {
        if !p.is_null() {
            unsafe { drop(std::ffi::CString::from_raw(p)) };
        }
    }
    fn demo_callbacks() -> ProviderCallbacks {
        ProviderCallbacks {
            struct_size: std::mem::size_of::<ProviderCallbacks>() as u32,
            list: Some(demo_list),
            stat: Some(demo_stat),
            read_to_file: Some(demo_read),
            free_string: Some(demo_free),
        }
    }

    #[test]
    fn custom_provider_end_to_end() {
        register_provider("demokit", &demo_callbacks(), std::ptr::null_mut()).unwrap();
        // Duplicate + built-in collision are distinct errors.
        assert_eq!(
            register_provider("demokit", &demo_callbacks(), std::ptr::null_mut())
                .unwrap_err()
                .0,
            ERR_DUPLICATE
        );
        assert_eq!(
            register_provider("file", &demo_callbacks(), std::ptr::null_mut())
                .unwrap_err()
                .0,
            ERR_DUPLICATE
        );
        let v = json::parse(&list("demokit://anything").unwrap()).unwrap();
        assert_eq!(v.get("entries").unwrap().as_array().unwrap().len(), 1);
        let st = stat("demokit://one.txt").unwrap();
        assert!(
            st.exists && st.size == 3 && st.provider == "demokit" && st.modified_unix == Some(42)
        );
        assert_eq!(read_text("demokit://one.txt", 10).unwrap(), "one");
        let td = TempDir::new("demoout");
        let dest = td.join("one.txt");
        copy("demokit://one.txt", &file_uri(&dest)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"one");
        // Custom as destination is not supported in v0.1.
        assert_eq!(
            copy("demokit://one.txt", "demokit://two.txt")
                .unwrap_err()
                .0,
            ERR_NOT_SUPPORTED
        );
        // Providers list now includes the custom scheme.
        let v = json::parse(&providers_json()).unwrap();
        let ps = v.get("providers").unwrap().as_array().unwrap();
        assert!(ps
            .iter()
            .any(|p| p.get("scheme").unwrap().as_str() == Some("demokit")));
        unregister_provider("demokit").unwrap();
        assert_eq!(
            parse_uri("demokit://x").unwrap_err().0,
            ERR_UNKNOWN_PROVIDER
        );
        assert_eq!(unregister_provider("demokit").unwrap_err().0, ERR_NOT_FOUND);
    }
}

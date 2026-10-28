//! C ABI surface — everything declared in include/providerkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `pv_free_string`.
//! - Functions returning a code return a PV_* code and set a
//!   thread-local last-error (code + message) on failure.
//!   `pv_read_text` returns its string directly and null on
//!   failure (last-error set), as its contract states.
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - All strings are UTF-8.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::catch_unwind;

use crate::{
    copy, list, mem_add_from_file, providers_json, read_text, read_to_file, register_provider,
    stat, unregister_provider, ProviderCallbacks, PvResult, ERR_INTERNAL, ERR_INVALID_ARG, PV_OK,
};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(PV_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> PvResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

fn out_string(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    match CString::new(text) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            clear_error();
            PV_OK as c_int
        }
        Err(_) => fail(ERR_INTERNAL, "result string contained a NUL byte"),
    }
}

fn code_of(r: PvResult<()>) -> c_int {
    match r {
        Ok(()) => {
            clear_error();
            PV_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn pv_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn pv_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn pv_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet.
#[no_mangle]
pub extern "C" fn pv_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

#[no_mangle]
pub extern "C" fn pv_list(uri: *const c_char, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match cstr(uri, "uri").and_then(list) {
            Ok(text) => out_string(out_json, text),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_stat(uri: *const c_char, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match cstr(uri, "uri").and_then(stat) {
            Ok(st) => out_string(out_json, st.to_json()),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_providers(out_json: *mut *mut c_char) -> c_int {
    match guard(|| out_string(out_json, providers_json())) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_read_to_file(uri: *const c_char, dest_local_path: *const c_char) -> c_int {
    match guard(|| {
        let r = (|| -> PvResult<()> {
            let u = cstr(uri, "uri")?;
            let d = cstr(dest_local_path, "dest_local_path")?;
            read_to_file(u, d)
        })();
        code_of(r)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Returns a caller-owned string (free with pv_free_string),
/// or null on failure with the last-error set.
#[no_mangle]
pub extern "C" fn pv_read_text(uri: *const c_char, max_bytes: u64) -> *mut c_char {
    match guard(|| -> *mut c_char {
        let r = cstr(uri, "uri").and_then(|u| read_text(u, max_bytes));
        match r {
            Ok(text) => match CString::new(text) {
                Ok(s) => {
                    clear_error();
                    s.into_raw()
                }
                Err(_) => {
                    set_error(ERR_INTERNAL, "result string contained a NUL byte");
                    std::ptr::null_mut()
                }
            },
            Err((code, msg)) => {
                set_error(code, &msg);
                std::ptr::null_mut()
            }
        }
    }) {
        Ok(p) => p,
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn pv_copy(src_uri: *const c_char, dst_uri: *const c_char) -> c_int {
    match guard(|| {
        let r = (|| -> PvResult<()> {
            let s = cstr(src_uri, "src_uri")?;
            let d = cstr(dst_uri, "dst_uri")?;
            copy(s, d)
        })();
        code_of(r)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_mem_add_from_file(
    name: *const c_char,
    inner_path: *const c_char,
    local_path: *const c_char,
) -> c_int {
    match guard(|| {
        let r = (|| -> PvResult<()> {
            let n = cstr(name, "name")?;
            let i = cstr(inner_path, "inner_path")?;
            let l = cstr(local_path, "local_path")?;
            mem_add_from_file(n, i, l)
        })();
        code_of(r)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_register_provider(
    scheme: *const c_char,
    callbacks: *const ProviderCallbacks,
    user_data: *mut c_void,
) -> c_int {
    match guard(|| {
        let r = (|| -> PvResult<()> {
            let s = cstr(scheme, "scheme")?;
            if callbacks.is_null() {
                return Err((ERR_INVALID_ARG, "callbacks must not be null".to_string()));
            }
            let cb = unsafe { &*callbacks };
            register_provider(s, cb, user_data)
        })();
        code_of(r)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pv_unregister_provider(scheme: *const c_char) -> c_int {
    match guard(|| {
        let r = (|| -> PvResult<()> {
            let s = cstr(scheme, "scheme")?;
            unregister_provider(s)
        })();
        code_of(r)
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ERR_NOT_FOUND, ERR_UNKNOWN_PROVIDER};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }
    fn take(p: *mut c_char) -> String {
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        pv_free_string(p);
        s
    }
    fn temp_file(tag: &str, content: &[u8]) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("providerkit-ffi-{tag}-{}-{n}", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn ffi_list_stat_providers_and_errors() {
        let local = temp_file("basic", b"ffi-content");
        let uri = cs(&format!("file://{local}"));
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(pv_stat(uri.as_ptr(), &mut out), PV_OK as c_int);
        let v = crate::json::parse(&take(out)).unwrap();
        assert_eq!(v.get("exists").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("size").unwrap().as_i64(), Some(11));
        assert_eq!(v.get("provider").unwrap().as_str(), Some("file"));

        assert_eq!(pv_providers(&mut out), PV_OK as c_int);
        let v = crate::json::parse(&take(out)).unwrap();
        assert!(v.get("providers").unwrap().as_array().is_some());

        // Unknown scheme via FFI: code + last_error agree.
        let bad = cs("wat://x");
        assert_eq!(
            pv_list(bad.as_ptr(), &mut out),
            ERR_UNKNOWN_PROVIDER as c_int
        );
        assert!(out.is_null());
        assert_eq!(pv_last_error_code(), ERR_UNKNOWN_PROVIDER as c_int);
        // Null arguments.
        assert_eq!(
            pv_list(std::ptr::null(), &mut out),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(
            pv_stat(uri.as_ptr(), std::ptr::null_mut()),
            ERR_INVALID_ARG as c_int
        );
        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn ffi_read_text_copy_and_mem() {
        let local = temp_file("text", b"ffi text");
        let uri = format!("file://{local}");
        let p = pv_read_text(cs(&uri).as_ptr(), 100);
        assert_eq!(take(p), "ffi text");
        // Over the cap: null + TOO_LARGE.
        let p = pv_read_text(cs(&uri).as_ptr(), 3);
        assert!(p.is_null());
        assert_eq!(pv_last_error_code(), crate::ERR_TOO_LARGE as c_int);
        // mem via FFI, then read back and copy out.
        assert_eq!(
            pv_mem_add_from_file(
                cs("ffistore").as_ptr(),
                cs("a/b.txt").as_ptr(),
                cs(&local).as_ptr()
            ),
            PV_OK as c_int
        );
        let p = pv_read_text(cs("mem://ffistore/a/b.txt").as_ptr(), 100);
        assert_eq!(take(p), "ffi text");
        let dest = temp_file("dest", b"");
        assert_eq!(
            pv_copy(
                cs("mem://ffistore/a/b.txt").as_ptr(),
                cs(&format!("file://{dest}")).as_ptr()
            ),
            PV_OK as c_int
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"ffi text");
        assert_eq!(
            pv_copy(
                cs("mem://ffistore/missing.txt").as_ptr(),
                cs(&format!("file://{dest}")).as_ptr()
            ),
            ERR_NOT_FOUND as c_int
        );
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_file(&dest);
    }

    extern "C" fn ffi_demo_list(
        _ud: *mut c_void,
        _uri: *const c_char,
        out: *mut *mut c_char,
    ) -> c_int {
        let s = CString::new("{\"entries\":[],\"provider\":\"ffidemo\"}").unwrap();
        unsafe { *out = s.into_raw() };
        PV_OK as c_int
    }
    extern "C" fn ffi_demo_stat(
        _ud: *mut c_void,
        _uri: *const c_char,
        out: *mut *mut c_char,
    ) -> c_int {
        let s =
            CString::new("{\"exists\":false,\"is_dir\":false,\"size\":0,\"modified_unix\":null}")
                .unwrap();
        unsafe { *out = s.into_raw() };
        PV_OK as c_int
    }
    extern "C" fn ffi_demo_free(_ud: *mut c_void, p: *mut c_char) {
        if !p.is_null() {
            unsafe { drop(CString::from_raw(p)) };
        }
    }

    #[test]
    fn ffi_register_custom_provider() {
        let cb = ProviderCallbacks {
            struct_size: std::mem::size_of::<ProviderCallbacks>() as u32,
            list: Some(ffi_demo_list),
            stat: Some(ffi_demo_stat),
            read_to_file: None,
            free_string: Some(ffi_demo_free),
        };
        assert_eq!(
            pv_register_provider(cs("ffidemo").as_ptr(), &cb, std::ptr::null_mut()),
            PV_OK as c_int
        );
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            pv_list(cs("ffidemo://root").as_ptr(), &mut out),
            PV_OK as c_int
        );
        assert!(take(out).contains("\"entries\":[]"));
        assert_eq!(
            pv_stat(cs("ffidemo://x").as_ptr(), &mut out),
            PV_OK as c_int
        );
        let v = crate::json::parse(&take(out)).unwrap();
        assert_eq!(v.get("provider").unwrap().as_str(), Some("ffidemo"));
        assert_eq!(v.get("exists").unwrap().as_bool(), Some(false));
        // No read_to_file callback: read is NOT_SUPPORTED.
        let dest = temp_file("nodest", b"");
        assert_eq!(
            pv_read_to_file(cs("ffidemo://x").as_ptr(), cs(&dest).as_ptr()),
            crate::ERR_NOT_SUPPORTED as c_int
        );
        // Null callbacks are invalid.
        assert_eq!(
            pv_register_provider(cs("other").as_ptr(), std::ptr::null(), std::ptr::null_mut()),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(
            pv_unregister_provider(cs("ffidemo").as_ptr()),
            PV_OK as c_int
        );
        let _ = std::fs::remove_file(&dest);
    }
}

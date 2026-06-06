//! `os` — process & environment access (Phase 4.4).
//!
//! Oracle parity with the interpreter's `os.exit`/`os.args`/`os.env`
//! (`src/interp/builtins.rs`).
//!
//! **Return-value ABI.** `os.args` returns `List<str>`, so it hands back a
//! concrete runtime [`La3List`] of [`La3Str`] (returned by value, the same
//! three-word-by-value convention as `str`). `os.env` returns `Option<str>`;
//! rather than commit the runtime to an `Option` *layout* (a tagged-union
//! decision the codegen owns, Phase 5/6), it uses the codegen-neutral
//! **`bool` + out-parameter** idiom: the result is `true` for `Some` (with the
//! value written through `out`) and `false` for `None`, and the codegen
//! assembles the `Option<str>` from the tag plus the `str`. `out` is always
//! left holding a valid (possibly empty) owned `str`, so the caller can read
//! and drop it unconditionally.

use crate::collections::{DropFn, La3List, la3_list_new, la3_list_push};
use crate::str::{La3Str, la3_str_drop};

/// Drop glue for a `str` element, type-erased to the collection [`DropFn`] ABI.
unsafe extern "C" fn str_drop_erased(p: *mut u8) {
    // SAFETY: `p` addresses one owned `La3Str` element.
    unsafe { la3_str_drop(p as *mut La3Str) };
}

/// Borrow a `str`'s bytes as a Rust `&str` (lossily-empty on null/invalid).
unsafe fn as_str<'a>(s: *const La3Str) -> &'a str {
    if s.is_null() {
        return "";
    }
    // SAFETY: `s` is live; a `La3Str` is always UTF-8.
    std::str::from_utf8(unsafe { (*s).as_bytes() }).unwrap_or("")
}

/// `os.exit(code)` — terminate the process. Never returns.
#[unsafe(no_mangle)]
pub extern "C" fn la3_os_exit(code: i32) -> ! {
    std::process::exit(code);
}

/// `os.args()` — the program arguments as a `List<str>`.
///
/// Mirrors the interpreter, which exposes the arguments *after* the program
/// name; for a compiled binary that is `std::env::args().skip(1)` (argv[0] is
/// the executable path).
#[unsafe(no_mangle)]
pub extern "C" fn la3_os_args() -> La3List {
    let mut list = la3_list_new(
        std::mem::size_of::<La3Str>(),
        std::mem::align_of::<La3Str>(),
        Some(str_drop_erased as DropFn),
    );
    for arg in std::env::args().skip(1) {
        let s = La3Str::from_bytes(arg.as_bytes());
        // The list copies the three words in and takes ownership of the buffer;
        // `s` itself has no destructor, so letting it fall out of scope is fine.
        unsafe { la3_list_push(&mut list, &s as *const La3Str as *const u8) };
    }
    list
}

/// `os.env(key)` → `Option<str>`, via the `bool` + out-parameter ABI: returns
/// `true` (Some) with the value in `*out`, or `false` (None) with `*out` empty.
///
/// # Safety
/// `key` is null or a live [`La3Str`]; `out` points at writable storage for one
/// `La3Str` (its previous contents, if any, are overwritten — the caller must
/// have dropped them).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_os_env(key: *const La3Str, out: *mut La3Str) -> bool {
    let k = unsafe { as_str(key) };
    match std::env::var(k) {
        Ok(v) => {
            unsafe { std::ptr::write(out, La3Str::from_bytes(v.as_bytes())) };
            true
        }
        Err(_) => {
            unsafe { std::ptr::write(out, La3Str::from_bytes(&[])) };
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collections::{la3_list_drop, la3_list_get, la3_list_len};
    use crate::str::{la3_str_drop, la3_str_len};

    #[test]
    fn args_returns_a_list_of_str() {
        // We cannot control the test harness's argv, but the call must return a
        // valid, droppable `List<str>` (length ≥ 0, every element a live str).
        let mut args = la3_os_args();
        let n = unsafe { la3_list_len(&args) };
        for i in 0..n {
            let p = unsafe { la3_list_get(&args, i) } as *const La3Str;
            // A non-negative byte length proves the element is a live `La3Str`.
            let _ = unsafe { la3_str_len(p) };
        }
        // Dropping the list runs each element's str drop-glue.
        unsafe { la3_list_drop(&mut args) };
    }

    #[test]
    fn env_present_and_absent() {
        // SAFETY (test): single-threaded; we set then read our own key.
        unsafe { std::env::set_var("LA3_TEST_ENV_KEY", "yes") };
        let key = unsafe { crate::str::la3_str_from_utf8("LA3_TEST_ENV_KEY".as_ptr(), 16) };
        let mut out = La3Str::from_bytes(&[]);
        let present = unsafe { la3_os_env(&key, &mut out) };
        assert!(present);
        assert_eq!(unsafe { la3_str_len(&out) }, 3); // "yes"
        unsafe { la3_str_drop(&mut out) };

        let missing = unsafe { crate::str::la3_str_from_utf8("LA3_NO_SUCH_KEY_X".as_ptr(), 17) };
        let mut out2 = La3Str::from_bytes(&[]);
        let absent = unsafe { la3_os_env(&missing, &mut out2) };
        assert!(!absent);
        assert_eq!(unsafe { la3_str_len(&out2) }, 0);

        let (mut key, mut missing) = (key, missing);
        unsafe {
            la3_str_drop(&mut out2);
            la3_str_drop(&mut key);
            la3_str_drop(&mut missing);
        }
    }
}

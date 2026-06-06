//! `io` — console output (Phase 4.4).
//!
//! The codegen renders a value to a `str` (the same rendering f-strings use,
//! Phase 4.3) and passes it here by **borrow** (`*const La3Str`): `io` is a
//! built-in, and built-ins borrow their arguments (borrowck, Phase 1.6.2), so
//! these functions never free the string — the caller still owns and later
//! drops it. The bytes are UTF-8 (a `La3Str` is always UTF-8) and are written
//! verbatim; the `*ln` variants append a single `\n`, mirroring the
//! interpreter's `println!`/`eprintln!`.
//!
//! An island like every stdlib module: it depends on nothing but the shared
//! `str` value type it is handed.

use std::io::Write;

use crate::str::La3Str;

/// Borrow the bytes of a (possibly null) `str`.
///
/// # Safety
/// `s` is null or points at a live [`La3Str`].
unsafe fn bytes<'a>(s: *const La3Str) -> &'a [u8] {
    if s.is_null() {
        &[]
    } else {
        // SAFETY: `s` is live; the borrow does not outlive the call.
        unsafe { (*s).as_bytes() }
    }
}

/// `io.print(s)` — write `s` to stdout with no trailing newline.
///
/// # Safety
/// `s` is null or points at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_io_print(s: *const La3Str) {
    let mut out = std::io::stdout();
    // Ignore write errors, as the interpreter's `print!` effectively does.
    let _ = out.write_all(unsafe { bytes(s) });
}

/// `io.println(s)` — write `s` to stdout followed by `\n`.
///
/// # Safety
/// `s` is null or points at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_io_println(s: *const La3Str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(unsafe { bytes(s) });
    let _ = out.write_all(b"\n");
}

/// `io.eprintln(s)` — write `s` to stderr followed by `\n`.
///
/// # Safety
/// `s` is null or points at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_io_eprintln(s: *const La3Str) {
    let mut err = std::io::stderr();
    let _ = err.write_all(unsafe { bytes(s) });
    let _ = err.write_all(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(s: &str) -> La3Str {
        unsafe { crate::str::la3_str_from_utf8(s.as_ptr(), s.len()) }
    }

    // We cannot easily capture the real stdout here, but we can prove the
    // functions accept a borrowed `str` without consuming it (the caller still
    // owns and drops it afterwards) and tolerate null/empty.
    #[test]
    fn print_borrows_and_does_not_consume() {
        let mut s = mk("hello");
        unsafe { la3_io_print(&s) };
        unsafe { la3_io_println(&s) };
        unsafe { la3_io_eprintln(&s) };
        // Still valid and owned after the calls — drop it ourselves.
        assert_eq!(unsafe { crate::str::la3_str_len(&s) }, 5);
        unsafe { crate::str::la3_str_drop(&mut s) };
    }

    #[test]
    fn null_and_empty_are_safe() {
        unsafe {
            la3_io_print(std::ptr::null());
            la3_io_println(std::ptr::null());
            la3_io_eprintln(std::ptr::null());
        }
        let mut e = mk("");
        unsafe { la3_io_println(&e) };
        unsafe { crate::str::la3_str_drop(&mut e) };
    }
}

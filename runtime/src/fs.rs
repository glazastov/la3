//! `fs` — filesystem access (Phase 4.4).
//!
//! Oracle parity with the interpreter's `fs.read`/`fs.write`
//! (`src/interp/builtins.rs`), including the error-message shape
//! `"{path}: {error}"`.
//!
//! **Return-value ABI.** Both functions return `Result<…>`. To avoid pinning
//! the runtime to a `Result` *layout* (the tagged-union representation is the
//! codegen's call, Phase 5/6), they use the codegen-neutral **`bool` +
//! out-parameter** idiom: the return is `true` for `Ok` and `false` for `Err`,
//! and the `str` payload (the file contents for a successful `read`, or the
//! error message otherwise) is written through `out`. The codegen assembles the
//! `Result<str>` / `Result<()>` from the tag plus the `str`. `out` always ends
//! holding a valid (possibly empty) owned `str`, so the caller can read and
//! drop it unconditionally.

use crate::str::La3Str;

/// Borrow a `str`'s bytes as a Rust `&str` (empty on null/invalid).
unsafe fn as_str<'a>(s: *const La3Str) -> &'a str {
    if s.is_null() {
        return "";
    }
    // SAFETY: `s` is live; a `La3Str` is always UTF-8.
    std::str::from_utf8(unsafe { (*s).as_bytes() }).unwrap_or("")
}

/// `fs.read(path)` → `Result<str>`: `true` with the file contents in `*out`, or
/// `false` with the error message (`"{path}: {error}"`) in `*out`.
///
/// # Safety
/// `path` is null or a live [`La3Str`]; `out` points at writable storage for one
/// `La3Str` (overwritten — the caller must have dropped any prior contents).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fs_read(path: *const La3Str, out: *mut La3Str) -> bool {
    let p = unsafe { as_str(path) };
    match std::fs::read_to_string(p) {
        Ok(contents) => {
            unsafe { std::ptr::write(out, La3Str::from_bytes(contents.as_bytes())) };
            true
        }
        Err(e) => {
            let msg = format!("{}: {}", p, e);
            unsafe { std::ptr::write(out, La3Str::from_bytes(msg.as_bytes())) };
            false
        }
    }
}

/// `fs.write(path, content)` → `Result<()>`: `true` on success (with `*out`
/// empty — the `Ok` payload is `()`), or `false` with the error message
/// (`"{path}: {error}"`) in `*out`.
///
/// # Safety
/// `path`/`content` are null or live [`La3Str`]s; `out` points at writable
/// storage for one `La3Str` (overwritten — the caller must have dropped any
/// prior contents).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_fs_write(
    path: *const La3Str,
    content: *const La3Str,
    out: *mut La3Str,
) -> bool {
    let p = unsafe { as_str(path) };
    let body = if content.is_null() {
        &[][..]
    } else {
        unsafe { (*content).as_bytes() }
    };
    match std::fs::write(p, body) {
        Ok(()) => {
            unsafe { std::ptr::write(out, La3Str::from_bytes(&[])) };
            true
        }
        Err(e) => {
            let msg = format!("{}: {}", p, e);
            unsafe { std::ptr::write(out, La3Str::from_bytes(msg.as_bytes())) };
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str::{la3_str_drop, la3_str_from_utf8, la3_str_len};

    fn mk(s: &str) -> La3Str {
        unsafe { la3_str_from_utf8(s.as_ptr(), s.len()) }
    }

    #[test]
    fn write_then_read_round_trips() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("la3_fs_test_{}.txt", std::process::id()));
        let path_s = dir.to_string_lossy().into_owned();

        let path = mk(&path_s);
        let content = mk("hello fs");

        let mut out = La3Str::from_bytes(&[]);
        let ok = unsafe { la3_fs_write(&path, &content, &mut out) };
        assert!(ok, "write should succeed");
        assert_eq!(unsafe { la3_str_len(&out) }, 0, "Ok(()) leaves out empty");
        unsafe { la3_str_drop(&mut out) };

        let mut read_out = La3Str::from_bytes(&[]);
        let ok = unsafe { la3_fs_read(&path, &mut read_out) };
        assert!(ok, "read should succeed");
        assert_eq!(read_out.as_bytes(), b"hello fs");
        unsafe { la3_str_drop(&mut read_out) };

        let _ = std::fs::remove_file(&path_s);
        let (mut path, mut content) = (path, content);
        unsafe {
            la3_str_drop(&mut path);
            la3_str_drop(&mut content);
        }
    }

    #[test]
    fn read_missing_file_is_err_with_path_prefixed_message() {
        let path = mk("/no/such/la3/path/here.txt");
        let mut out = La3Str::from_bytes(&[]);
        let ok = unsafe { la3_fs_read(&path, &mut out) };
        assert!(!ok, "missing file is Err");
        let msg = std::str::from_utf8(out.as_bytes()).unwrap();
        assert!(
            msg.starts_with("/no/such/la3/path/here.txt: "),
            "message is path-prefixed (oracle shape), got {msg:?}"
        );
        let mut path = path;
        unsafe {
            la3_str_drop(&mut out);
            la3_str_drop(&mut path);
        }
    }
}

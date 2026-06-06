//! `bytes` — byte-buffer helpers (Phase 4.4, subset).
//!
//! `bytes.to_hex(&[u8]) -> str` only, mirroring the interpreter oracle
//! (`call_module("bytes", "to_hex", …)`): lowercase, two hex digits per byte,
//! no separator. The input is a borrowed byte slice, lowered as a `(ptr, len)`
//! pair; the result is a fresh owned `str`.
//!
//! The reference's other `bytes` functions (`from_hex`/`from_base64`/
//! `to_base64`/`compare`) are deferred: `from_hex`/`from_base64` return
//! `Result<List<u8>>` — an aggregate whose return layout is the codegen's call
//! (Phase 5/6) — and `to_base64`/`compare` have no interpreter oracle to
//! differentially test against yet.

use std::fmt::Write as _;

use crate::str::La3Str;

/// `bytes.to_hex(b)` — lowercase hex of `len` bytes at `data`.
///
/// # Safety
/// `data` points at `len` readable bytes (or is null with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_bytes_to_hex(data: *const u8, len: usize) -> La3Str {
    if len == 0 {
        return La3Str::from_bytes(&[]);
    }
    // SAFETY: caller guarantees `len` readable bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let mut s = String::with_capacity(len * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    La3Str::from_bytes(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str::{la3_str_drop, la3_str_len};

    #[test]
    fn to_hex_is_lowercase_two_digits_each() {
        let data = [0x00u8, 0x0f, 0xff, 0xa5];
        let mut hex = unsafe { la3_bytes_to_hex(data.as_ptr(), data.len()) };
        assert_eq!(hex.as_bytes(), b"000fffa5");
        unsafe { la3_str_drop(&mut hex) };
    }

    #[test]
    fn empty_input_is_empty_string() {
        let mut hex = unsafe { la3_bytes_to_hex(std::ptr::null(), 0) };
        assert_eq!(unsafe { la3_str_len(&hex) }, 0);
        unsafe { la3_str_drop(&mut hex) };
    }
}

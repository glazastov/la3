//! `str` — the owned, heap-backed UTF-8 string the compiled code uses (Phase 4.1).
//!
//! Memory model: **ownership**, not reference counting. A `str` value *is* the
//! [`La3Str`] triple `{ ptr, len, cap }` (the same shape as Rust's `String`),
//! moved by copying those three words and freed by [`la3_str_drop`] — which is
//! exactly what a MIR `Statement::Drop` of a `str` lowers to (Phase 3.5 inserts
//! the drop; Phase 5/6 codegen emits the call). The buffer is UTF-8 (source text
//! is UTF-8, reference §1), and `la3_str_len` returns the **byte** length, to
//! match the interpreter oracle (`String::len`).
//!
//! Everything is `#[repr(C)]`/`extern "C"` so the codegen can name and lay these
//! out directly. The buffer is allocated and freed through `Vec<u8>`, so it uses
//! the one global allocator consistently.

use std::ptr::NonNull;

/// An owned UTF-8 string: a pointer to a heap buffer plus its byte length and
/// capacity. This is a plain value (no destructor of its own) so the codegen can
/// move it by copying the three words; ownership is released explicitly by
/// [`la3_str_drop`]. An empty string holds a dangling, non-null `ptr` with
/// `len == cap == 0` and owns no allocation.
#[repr(C)]
pub struct La3Str {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl La3Str {
    /// The empty string: no allocation, a dangling but non-null pointer (so it is
    /// never confused with a null/moved-from slot).
    fn empty() -> La3Str {
        La3Str {
            ptr: NonNull::<u8>::dangling().as_ptr(),
            len: 0,
            cap: 0,
        }
    }

    /// Take ownership of `v`'s buffer as a `La3Str` (no copy).
    fn from_vec(mut v: Vec<u8>) -> La3Str {
        if v.capacity() == 0 {
            return La3Str::empty();
        }
        let s = La3Str {
            ptr: v.as_mut_ptr(),
            len: v.len(),
            cap: v.capacity(),
        };
        std::mem::forget(v);
        s
    }

    /// Copy `bytes` into a fresh owned buffer.
    pub(crate) fn from_bytes(bytes: &[u8]) -> La3Str {
        if bytes.is_empty() {
            return La3Str::empty();
        }
        La3Str::from_vec(bytes.to_vec())
    }

    /// The UTF-8 bytes, borrowed for the lifetime of `&self`.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            // SAFETY: a non-empty `La3Str` owns `len` initialized bytes at `ptr`.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    /// Free the owned buffer (no-op for the empty string).
    ///
    /// # Safety
    /// Must be called at most once per allocation; `self` is consumed.
    unsafe fn free(self) {
        if self.cap != 0 {
            // SAFETY: produced by `from_bytes`/`Vec`, so these parts reconstruct
            // the original allocation, which is then dropped.
            drop(unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) });
        }
    }
}

/// Build an owned `str` by copying `len` UTF-8 bytes from `data` (a string
/// literal the codegen emits into read-only data). `data` may be null iff
/// `len == 0`.
///
/// # Safety
/// `data` must point at `len` readable, valid-UTF-8 bytes (or be null with
/// `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_from_utf8(data: *const u8, len: usize) -> La3Str {
    if len == 0 {
        return La3Str::empty();
    }
    // SAFETY: caller guarantees `len` readable bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    La3Str::from_bytes(bytes)
}

/// Drop glue for a `str`: free the buffer the slot at `s` owns, then poison the
/// slot (an empty string) so an accidental second drop is a safe no-op. This is
/// what a MIR `drop(place)` of a `str` lowers to.
///
/// # Safety
/// `s` must be null or point at a live, owned [`La3Str`] slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_drop(s: *mut La3Str) {
    if s.is_null() {
        return;
    }
    // SAFETY: move the value out, leave an inert empty string behind.
    let owned = unsafe { std::ptr::read(s) };
    unsafe { std::ptr::write(s, La3Str::empty()) };
    unsafe { owned.free() };
}

/// The byte length of a `str` (matches the interpreter's `str.len()`).
///
/// # Safety
/// `s` must be null or point at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_len(s: *const La3Str) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: `s` points at a live value.
    unsafe { (*s).len }
}

/// Bytewise equality of two `str`s (backs `==` and `str` match patterns).
///
/// # Safety
/// `a` and `b` must each be null or point at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_eq(a: *const La3Str, b: *const La3Str) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => return true,
        (true, _) | (_, true) => return false,
        _ => {}
    }
    // SAFETY: both are live values.
    unsafe { (*a).as_bytes() == (*b).as_bytes() }
}

/// `a + b`: a fresh owned `str` that is the concatenation. Neither input is
/// consumed (the caller still owns and later drops `a` and `b`).
///
/// # Safety
/// `a` and `b` must each be null or point at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_concat(a: *const La3Str, b: *const La3Str) -> La3Str {
    let av = if a.is_null() { &[][..] } else { unsafe { (*a).as_bytes() } };
    let bv = if b.is_null() { &[][..] } else { unsafe { (*b).as_bytes() } };
    if av.is_empty() && bv.is_empty() {
        return La3Str::empty();
    }
    let mut v = Vec::with_capacity(av.len() + bv.len());
    v.extend_from_slice(av);
    v.extend_from_slice(bv);
    La3Str::from_vec(v)
}

/// A deep, independent copy of a `str` (used when a value must be duplicated).
///
/// # Safety
/// `s` must be null or point at a live [`La3Str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_str_clone(s: *const La3Str) -> La3Str {
    if s.is_null() {
        return La3Str::empty();
    }
    // SAFETY: `s` is live.
    La3Str::from_bytes(unsafe { (*s).as_bytes() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `La3Str` from a Rust `&str` through the public ABI.
    fn mk(s: &str) -> La3Str {
        unsafe { la3_str_from_utf8(s.as_ptr(), s.len()) }
    }
    fn bytes(s: &La3Str) -> &[u8] {
        s.as_bytes()
    }

    #[test]
    fn from_utf8_round_trips_bytes_and_len() {
        let mut s = mk("héllo"); // 6 bytes (é is 2)
        assert_eq!(unsafe { la3_str_len(&s) }, 6);
        assert_eq!(bytes(&s), "héllo".as_bytes());
        unsafe { la3_str_drop(&mut s) };
    }

    #[test]
    fn empty_owns_nothing_and_drops_cleanly() {
        let mut e = mk("");
        assert_eq!(unsafe { la3_str_len(&e) }, 0);
        assert_eq!(e.cap, 0);
        unsafe { la3_str_drop(&mut e) }; // no allocation to free
        // A second drop is a safe no-op (slot was poisoned to empty).
        unsafe { la3_str_drop(&mut e) };
    }

    #[test]
    fn eq_is_bytewise() {
        let mut a = mk("abc");
        let mut b = mk("abc");
        let mut c = mk("abd");
        assert!(unsafe { la3_str_eq(&a, &b) });
        assert!(!unsafe { la3_str_eq(&a, &c) });
        unsafe {
            la3_str_drop(&mut a);
            la3_str_drop(&mut b);
            la3_str_drop(&mut c);
        }
    }

    #[test]
    fn concat_builds_a_fresh_owned_string() {
        let a = mk("foo");
        let b = mk("bar");
        let mut ab = unsafe { la3_str_concat(&a, &b) };
        assert_eq!(bytes(&ab), b"foobar");
        // Inputs are untouched by concat — still independently valid and owned.
        assert_eq!(bytes(&a), b"foo");
        assert_eq!(bytes(&b), b"bar");
        let (mut a, mut b) = (a, b);
        unsafe {
            la3_str_drop(&mut ab);
            la3_str_drop(&mut a);
            la3_str_drop(&mut b);
        }
    }

    #[test]
    fn clone_is_independent() {
        let mut s = mk("data");
        let mut c = unsafe { la3_str_clone(&s) };
        assert_eq!(bytes(&c), b"data");
        // Different buffers: dropping one leaves the other valid.
        assert_ne!(s.ptr, c.ptr);
        unsafe { la3_str_drop(&mut s) };
        assert_eq!(bytes(&c), b"data");
        unsafe { la3_str_drop(&mut c) };
    }

    #[test]
    fn concat_with_empty_is_identity() {
        let e = mk("");
        let x = mk("x");
        let mut r = unsafe { la3_str_concat(&e, &x) };
        assert_eq!(bytes(&r), b"x");
        let (mut e, mut x) = (e, x);
        unsafe {
            la3_str_drop(&mut r);
            la3_str_drop(&mut e);
            la3_str_drop(&mut x);
        }
    }

    #[test]
    fn null_pointers_are_handled() {
        assert_eq!(unsafe { la3_str_len(std::ptr::null()) }, 0);
        assert!(unsafe { la3_str_eq(std::ptr::null(), std::ptr::null()) });
        unsafe { la3_str_drop(std::ptr::null_mut()) }; // no-op
        let mut empty = unsafe { la3_str_clone(std::ptr::null()) };
        assert_eq!(unsafe { la3_str_len(&empty) }, 0);
        unsafe { la3_str_drop(&mut empty) };
    }
}

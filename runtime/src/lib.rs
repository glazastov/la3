//! La3 native runtime.
//!
//! Compiled La3 programs do not carry their heap types or standard library in
//! the generated LLVM IR; instead the codegen emits calls into this crate,
//! which is linked in as a static library. This is the counterpart of the
//! builtins in `src/interp.rs`, reimplemented for ahead-of-time compilation.
//!
//! Phase 0 establishes only the skeleton: the reference-counted heap header and
//! the value tag enum that later phases (3 and 5) will flesh out. Everything
//! here is `extern "C"` so the codegen can name these symbols directly.
//!
//! Memory model (v1): **ARC**. Every heap allocation begins with an [`RcHeader`]
//! whose `strong` count is bumped by `la3_rc_inc` and dropped by `la3_rc_dec`;
//! when it reaches zero the object's destructor runs and the block is freed.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

/// Runtime tag for a heap object, so a single `rc_dec` can dispatch to the
/// right destructor. Mirrors the heap-allocated arms of the interpreter's
/// `Value` (see `src/interp.rs`). Extended as Phase 3/5 land each type.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag {
    Str = 0,
    List = 1,
    Map = 2,
    Set = 3,
    Struct = 4,
    Enum = 5,
    Closure = 6,
}

/// The header every reference-counted heap block starts with. The payload
/// follows immediately after, so codegen lays out `{ RcHeader, <fields...> }`.
#[repr(C)]
pub struct RcHeader {
    /// Strong reference count. `AtomicUsize` keeps it correct once Phase 9
    /// hands heap values across threads; single-threaded code pays only a
    /// relaxed add.
    pub strong: AtomicUsize,
    pub tag: Tag,
}

impl RcHeader {
    pub fn new(tag: Tag) -> Self {
        RcHeader { strong: AtomicUsize::new(1), tag }
    }
}

/// Increment the strong count of a heap object. No-op on null.
///
/// # Safety
/// `ptr` must be null or point at a live block whose first field is an
/// [`RcHeader`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_rc_inc(ptr: *const RcHeader) {
    if ptr.is_null() {
        return;
    }
    unsafe { (*ptr).strong.fetch_add(1, Ordering::Relaxed) };
}

/// Decrement the strong count; returns `true` when it reached zero (the caller's
/// codegen is then responsible for running the destructor and freeing). No-op on
/// null. Destructor dispatch by [`Tag`] arrives with the heap types in Phase 3/5.
///
/// # Safety
/// Same contract as [`la3_rc_inc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_rc_dec(ptr: *const RcHeader) -> bool {
    if ptr.is_null() {
        return false;
    }
    let prev = unsafe { (*ptr).strong.fetch_sub(1, Ordering::Release) };
    prev == 1
}

/// Smoke-test symbol used by Phase 0.5's differential harness to confirm the
/// runtime links. Returns the La3 version so a `build`'d binary can prove the
/// whole toolchain (codegen → object → link → run) is wired before any real
/// codegen exists.
#[unsafe(no_mangle)]
pub extern "C" fn la3_runtime_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_inc_dec_reaches_zero() {
        let h = RcHeader::new(Tag::Str);
        let p: *const RcHeader = &h;
        unsafe {
            la3_rc_inc(p); // strong = 2
            assert!(!la3_rc_dec(p)); // 2 -> 1, not last
            assert!(la3_rc_dec(p)); // 1 -> 0, last
        }
    }

    #[test]
    fn null_is_a_noop() {
        unsafe {
            la3_rc_inc(std::ptr::null());
            assert!(!la3_rc_dec(std::ptr::null()));
        }
    }
}

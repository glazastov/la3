//! `List`, `Map`, `Set` — the owned heap collections (Phase 4.2).
//!
//! Like [`crate::str`] these follow the **ownership** model: a collection owns
//! its elements, and `drop` frees the buffer *after* running each element's
//! drop-glue, so dropping a `List<str>` frees every `str` too. There is one
//! linked runtime for every monomorphization, so the collections are
//! **type-erased**: an element is `stride` bytes, and the codegen supplies, at
//! construction, the element's size/alignment plus an optional per-element
//! **drop-glue** function pointer (`None`/null for `Copy` elements) and — for the
//! keyed collections — an **equality** function pointer.
//!
//! `Map`/`Set` use a linear scan for lookup. That is O(n), but it is *correct*
//! for the ownership/drop contract this subpart is about; a hashed
//! implementation is a later, behaviour-preserving optimization (and exactly the
//! kind of swappable impl the à-la-carte-stdlib pillar anticipates). Element
//! moves are bytewise: the codegen moves a value out and passes its address, and
//! the collection takes ownership by copying the bytes in (and drops the incoming
//! value itself when a key/element turns out to be a duplicate).
//!
//! All of this is `unsafe` and `extern "C"`; the unsafe allocation is centralized
//! in [`Raw`], the type-erased growable buffer the three collections share.

use std::alloc::{Layout, alloc, dealloc, realloc};

/// Per-element drop-glue (e.g. `la3_str_drop`); `None` for `Copy` elements.
pub type DropFn = unsafe extern "C" fn(*mut u8);
/// Bytewise-or-typed equality of two elements (e.g. `la3_str_eq`).
pub type EqFn = unsafe extern "C" fn(*const u8, *const u8) -> bool;

// ---------------------------------------------------------------------------
// Raw — a type-erased growable buffer (the shared core)
// ---------------------------------------------------------------------------

/// A growable buffer of `len` elements, each `stride` bytes and `align`-aligned.
/// Owns its allocation; [`Raw::drop_all`] runs `drop_elem` on every element then
/// frees the block. An element `size` is always a multiple of its `align`, so
/// `ptr + i*stride` is correctly aligned for every `i`.
#[repr(C)]
struct Raw {
    ptr: *mut u8,
    len: usize,
    cap: usize,
    stride: usize,
    align: usize,
    drop_elem: Option<DropFn>,
}

impl Raw {
    fn new(stride: usize, align: usize, drop_elem: Option<DropFn>) -> Raw {
        Raw {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
            stride: stride.max(1),
            align: align.max(1),
            drop_elem,
        }
    }

    fn layout(&self, cap: usize) -> Layout {
        Layout::from_size_align(self.stride * cap, self.align).expect("valid layout")
    }

    /// Ensure room for at least one more element, growing (4, then ×2) as needed.
    fn reserve_one(&mut self) {
        if self.len < self.cap {
            return;
        }
        let new_cap = if self.cap == 0 { 4 } else { self.cap * 2 };
        // SAFETY: `ptr`/`cap` describe the current allocation (or null at cap 0).
        let new_ptr = unsafe {
            if self.cap == 0 {
                alloc(self.layout(new_cap))
            } else {
                realloc(self.ptr, self.layout(self.cap), self.stride * new_cap)
            }
        };
        assert!(!new_ptr.is_null(), "la3 runtime: collection allocation failed");
        self.ptr = new_ptr;
        self.cap = new_cap;
    }

    /// The address of element `i` (no bounds check — callers pass `i < len`).
    fn at(&self, i: usize) -> *mut u8 {
        // SAFETY: within an allocation of `cap >= len` elements.
        unsafe { self.ptr.add(i * self.stride) }
    }

    /// Append by copying `stride` bytes from `src` (ownership transfers in).
    ///
    /// # Safety
    /// `src` must point at `stride` initialized bytes of a moved-from value.
    unsafe fn push(&mut self, src: *const u8) {
        self.reserve_one();
        // SAFETY: `at(len)` is uninitialized room for one element.
        unsafe { std::ptr::copy_nonoverlapping(src, self.at(self.len), self.stride) };
        self.len += 1;
    }

    /// Run `drop_elem` on every element (if any), then free the buffer.
    fn drop_all(&mut self) {
        if let Some(d) = self.drop_elem {
            for i in 0..self.len {
                // SAFETY: element `i` is initialized and owned.
                unsafe { d(self.at(i)) };
            }
        }
        if self.cap != 0 {
            // SAFETY: `ptr`/`cap` describe this allocation.
            unsafe { dealloc(self.ptr, self.layout(self.cap)) };
        }
        self.ptr = std::ptr::null_mut();
        self.len = 0;
        self.cap = 0;
    }

    /// Drop just element `i`'s value in place (used when a duplicate insert hands
    /// us ownership of a value we will not store).
    ///
    /// # Safety
    /// `i < len`; the element is not used afterwards.
    unsafe fn drop_one_at(&self, p: *mut u8) {
        if let Some(d) = self.drop_elem {
            // SAFETY: `p` is an owned element of this element type.
            unsafe { d(p) };
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// An owned, growable, ordered list (`List<T>`).
#[repr(C)]
pub struct La3List {
    raw: Raw,
}

/// Create an empty `List<T>` whose elements are `elem_size` bytes,
/// `elem_align`-aligned, with `drop_elem` glue (null for `Copy` elements).
#[unsafe(no_mangle)]
pub extern "C" fn la3_list_new(
    elem_size: usize,
    elem_align: usize,
    drop_elem: Option<DropFn>,
) -> La3List {
    La3List {
        raw: Raw::new(elem_size, elem_align, drop_elem),
    }
}

/// Append an element by moving its bytes in from `elem`.
///
/// # Safety
/// `list` is live; `elem` points at one moved-from element value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_list_push(list: *mut La3List, elem: *const u8) {
    // SAFETY: caller guarantees a live list and a valid element.
    unsafe { (*list).raw.push(elem) };
}

/// The address of element `index` (for reads and in-place writes).
///
/// # Safety
/// `list` is live and `index < len` (the bounds check is the codegen's job, as
/// in the interpreter).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_list_get(list: *const La3List, index: usize) -> *mut u8 {
    // SAFETY: caller guarantees the index is in range.
    unsafe { (*list).raw.at(index) }
}

/// The number of elements.
///
/// # Safety
/// `list` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_list_len(list: *const La3List) -> usize {
    if list.is_null() {
        return 0;
    }
    // SAFETY: live list.
    unsafe { (*list).raw.len }
}

/// Drop glue for a `List`: drop every element, then free the buffer.
///
/// # Safety
/// `list` is null or points at a live, owned `List`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_list_drop(list: *mut La3List) {
    if list.is_null() {
        return;
    }
    // SAFETY: live list; afterwards the slot holds an empty buffer.
    unsafe { (*list).raw.drop_all() };
}

// ---------------------------------------------------------------------------
// Set
// ---------------------------------------------------------------------------

/// An owned set of unique elements (`Set<T>`), deduplicated by `eq`.
#[repr(C)]
pub struct La3Set {
    raw: Raw,
    eq: EqFn,
}

/// Create an empty `Set<T>`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_set_new(
    elem_size: usize,
    elem_align: usize,
    drop_elem: Option<DropFn>,
    eq: EqFn,
) -> La3Set {
    La3Set {
        raw: Raw::new(elem_size, elem_align, drop_elem),
        eq,
    }
}

/// Insert `elem` (moving it in). Returns `true` if newly inserted; on a duplicate
/// the incoming value is dropped (its ownership was handed to us) and `false` is
/// returned.
///
/// # Safety
/// `set` is live; `elem` points at one moved-from element value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_set_insert(set: *mut La3Set, elem: *const u8) -> bool {
    // SAFETY: live set.
    let s = unsafe { &mut *set };
    for i in 0..s.raw.len {
        // SAFETY: element `i` is initialized; `elem` is a valid value of the type.
        if unsafe { (s.eq)(s.raw.at(i), elem) } {
            // Duplicate: we own the incoming value but will not store it.
            unsafe { s.raw.drop_one_at(elem as *mut u8) };
            return false;
        }
    }
    // SAFETY: not present — take ownership by appending.
    unsafe { s.raw.push(elem) };
    true
}

/// Whether `elem` is present.
///
/// # Safety
/// `set` is live; `elem` is a valid element value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_set_contains(set: *const La3Set, elem: *const u8) -> bool {
    // SAFETY: live set.
    let s = unsafe { &*set };
    (0..s.raw.len).any(|i| unsafe { (s.eq)(s.raw.at(i), elem) })
}

/// The number of elements.
///
/// # Safety
/// `set` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_set_len(set: *const La3Set) -> usize {
    if set.is_null() {
        return 0;
    }
    // SAFETY: live set.
    unsafe { (*set).raw.len }
}

/// Drop glue for a `Set`.
///
/// # Safety
/// `set` is null or points at a live, owned `Set`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_set_drop(set: *mut La3Set) {
    if set.is_null() {
        return;
    }
    // SAFETY: live set.
    unsafe { (*set).raw.drop_all() };
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

/// An owned key→value map (`Map<K, V>`), keyed by `eq`. Keys and values live in
/// two parallel buffers kept the same length.
#[repr(C)]
pub struct La3Map {
    keys: Raw,
    vals: Raw,
    eq: EqFn,
}

/// Create an empty `Map<K, V>`.
#[unsafe(no_mangle)]
pub extern "C" fn la3_map_new(
    key_size: usize,
    key_align: usize,
    key_drop: Option<DropFn>,
    val_size: usize,
    val_align: usize,
    val_drop: Option<DropFn>,
    eq: EqFn,
) -> La3Map {
    La3Map {
        keys: Raw::new(key_size, key_align, key_drop),
        vals: Raw::new(val_size, val_align, val_drop),
        eq,
    }
}

/// Insert or update `key → val` (moving both in). On an existing key the old
/// value is dropped and replaced, and the incoming *key* (a duplicate) is
/// dropped; otherwise key and value are appended.
///
/// # Safety
/// `map` is live; `key`/`val` point at moved-from values of the key/value types.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_map_insert(map: *mut La3Map, key: *const u8, val: *const u8) {
    // SAFETY: live map.
    let m = unsafe { &mut *map };
    for i in 0..m.keys.len {
        // SAFETY: key `i` is initialized; `key` is a valid key value.
        if unsafe { (m.eq)(m.keys.at(i), key) } {
            // Replace the value: drop the old one, copy the new one in place.
            let slot = m.vals.at(i);
            unsafe {
                m.vals.drop_one_at(slot);
                std::ptr::copy_nonoverlapping(val, slot, m.vals.stride);
                // The incoming key duplicates the stored one; drop it.
                m.keys.drop_one_at(key as *mut u8);
            }
            return;
        }
    }
    // New entry.
    unsafe {
        m.keys.push(key);
        m.vals.push(val);
    }
}

/// The address of the value for `key`, or null if absent (backs `.get` → Option).
///
/// # Safety
/// `map` is live; `key` is a valid key value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_map_get(map: *const La3Map, key: *const u8) -> *mut u8 {
    // SAFETY: live map.
    let m = unsafe { &*map };
    for i in 0..m.keys.len {
        if unsafe { (m.eq)(m.keys.at(i), key) } {
            return m.vals.at(i);
        }
    }
    std::ptr::null_mut()
}

/// Whether `key` is present.
///
/// # Safety
/// `map` is live; `key` is a valid key value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_map_contains(map: *const La3Map, key: *const u8) -> bool {
    // SAFETY: live map.
    !unsafe { la3_map_get(map, key) }.is_null()
}

/// The number of entries.
///
/// # Safety
/// `map` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_map_len(map: *const La3Map) -> usize {
    if map.is_null() {
        return 0;
    }
    // SAFETY: live map.
    unsafe { (*map).keys.len }
}

/// Drop glue for a `Map`: drop every key and value, then free both buffers.
///
/// # Safety
/// `map` is null or points at a live, owned `Map`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn la3_map_drop(map: *mut La3Map) {
    if map.is_null() {
        return;
    }
    // SAFETY: live map.
    unsafe {
        (*map).keys.drop_all();
        (*map).vals.drop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- helpers: an i32 element (Copy, no drop) and eq -----------------------

    unsafe extern "C" fn i32_eq(a: *const u8, b: *const u8) -> bool {
        unsafe { *(a as *const i32) == *(b as *const i32) }
    }
    fn push_i32(list: *mut La3List, v: i32) {
        unsafe { la3_list_push(list, &v as *const i32 as *const u8) };
    }
    fn get_i32(list: *const La3List, i: usize) -> i32 {
        unsafe { *(la3_list_get(list, i) as *const i32) }
    }

    // -- a "tracked" element whose drop bumps a global counter ----------------

    static DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn tracked_drop(_p: *mut u8) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn u64_eq(a: *const u8, b: *const u8) -> bool {
        unsafe { *(a as *const u64) == *(b as *const u64) }
    }

    #[test]
    fn list_push_get_len() {
        let mut l = la3_list_new(4, 4, None);
        for v in [10, 20, 30] {
            push_i32(&mut l, v);
        }
        assert_eq!(unsafe { la3_list_len(&l) }, 3);
        assert_eq!(get_i32(&l, 0), 10);
        assert_eq!(get_i32(&l, 2), 30);
        unsafe { la3_list_drop(&mut l) };
        assert_eq!(unsafe { la3_list_len(&l) }, 0);
    }

    #[test]
    fn list_grows_past_initial_capacity() {
        let mut l = la3_list_new(4, 4, None);
        for v in 0..100 {
            push_i32(&mut l, v);
        }
        assert_eq!(unsafe { la3_list_len(&l) }, 100);
        assert_eq!(get_i32(&l, 99), 99);
        unsafe { la3_list_drop(&mut l) };
    }

    #[test]
    fn list_drop_runs_element_glue_once_each() {
        DROPS.store(0, Ordering::SeqCst);
        let mut l = la3_list_new(8, 8, Some(tracked_drop));
        for v in 0u64..5 {
            unsafe { la3_list_push(&mut l, &v as *const u64 as *const u8) };
        }
        unsafe { la3_list_drop(&mut l) };
        assert_eq!(DROPS.load(Ordering::SeqCst), 5, "each element dropped once");
    }

    #[test]
    fn set_dedups_and_drops_the_duplicate() {
        DROPS.store(0, Ordering::SeqCst);
        let mut s = la3_set_new(8, 8, Some(tracked_drop), u64_eq);
        let a: u64 = 7;
        assert!(unsafe { la3_set_insert(&mut s, &a as *const u64 as *const u8) });
        // Re-insert the same value: rejected, and the incoming copy is dropped now.
        assert!(!unsafe { la3_set_insert(&mut s, &a as *const u64 as *const u8) });
        assert_eq!(DROPS.load(Ordering::SeqCst), 1, "the duplicate was dropped");
        assert_eq!(unsafe { la3_set_len(&s) }, 1);
        assert!(unsafe { la3_set_contains(&s, &a as *const u64 as *const u8) });
        let b: u64 = 8;
        assert!(!unsafe { la3_set_contains(&s, &b as *const u64 as *const u8) });
        unsafe { la3_set_drop(&mut s) };
        assert_eq!(DROPS.load(Ordering::SeqCst), 2, "stored element dropped at the end");
    }

    #[test]
    fn map_insert_get_and_update() {
        let mut m = la3_map_new(4, 4, None, 4, 4, None, i32_eq);
        let (k1, v1): (i32, i32) = (1, 100);
        let (k2, v2): (i32, i32) = (2, 200);
        unsafe {
            la3_map_insert(&mut m, &k1 as *const _ as *const u8, &v1 as *const _ as *const u8);
            la3_map_insert(&mut m, &k2 as *const _ as *const u8, &v2 as *const _ as *const u8);
        }
        assert_eq!(unsafe { la3_map_len(&m) }, 2);
        let got = unsafe { la3_map_get(&m, &k2 as *const _ as *const u8) };
        assert_eq!(unsafe { *(got as *const i32) }, 200);
        // Update existing key — length unchanged, value replaced.
        let v2b: i32 = 222;
        unsafe { la3_map_insert(&mut m, &k2 as *const _ as *const u8, &v2b as *const _ as *const u8) };
        assert_eq!(unsafe { la3_map_len(&m) }, 2);
        let got = unsafe { la3_map_get(&m, &k2 as *const _ as *const u8) };
        assert_eq!(unsafe { *(got as *const i32) }, 222);
        // Absent key → null (Option::None).
        let k3: i32 = 9;
        assert!(unsafe { la3_map_get(&m, &k3 as *const _ as *const u8) }.is_null());
        assert!(!unsafe { la3_map_contains(&m, &k3 as *const _ as *const u8) });
        unsafe { la3_map_drop(&mut m) };
    }

    #[test]
    fn map_update_drops_old_value_and_duplicate_key() {
        DROPS.store(0, Ordering::SeqCst);
        // Keys are Copy (u64, no drop); values carry the tracked drop glue.
        let mut m = la3_map_new(8, 8, None, 8, 8, Some(tracked_drop), u64_eq);
        let k: u64 = 42;
        let v1: u64 = 1;
        let v2: u64 = 2;
        unsafe {
            la3_map_insert(&mut m, &k as *const _ as *const u8, &v1 as *const _ as *const u8);
            // Update: the old value is dropped now.
            la3_map_insert(&mut m, &k as *const _ as *const u8, &v2 as *const _ as *const u8);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), 1, "old value dropped on update");
        unsafe { la3_map_drop(&mut m) };
        assert_eq!(DROPS.load(Ordering::SeqCst), 2, "surviving value dropped at the end");
    }

    #[test]
    fn null_pointers_are_handled() {
        assert_eq!(unsafe { la3_list_len(std::ptr::null()) }, 0);
        assert_eq!(unsafe { la3_set_len(std::ptr::null()) }, 0);
        assert_eq!(unsafe { la3_map_len(std::ptr::null()) }, 0);
        unsafe {
            la3_list_drop(std::ptr::null_mut());
            la3_set_drop(std::ptr::null_mut());
            la3_map_drop(std::ptr::null_mut());
        }
    }
}

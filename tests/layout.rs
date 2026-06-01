//! Phase 1.3 — by-value type layout.
//!
//! Drives `la3 layout` and asserts the computed size, alignment, and field
//! offsets of structs and enums. Layout is C-style (fields in order at their
//! natural alignment, the whole rounded to its alignment); enums are tagged
//! unions (a 1-byte discriminant followed by the largest variant payload).

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Write `src` to a temp file and return the stdout of `la3 layout`.
fn layout(src: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_layout_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["layout", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "layout failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Assert every needle appears in the layout dump (order-independent).
fn has(src: &str, needles: &[&str]) {
    let out = layout(src);
    for n in needles {
        assert!(out.contains(n), "expected {:?} in layout:\n{}", n, out);
    }
}

#[test]
fn two_f64_struct_is_16_bytes() {
    has(
        "struct Point { x: f64, y: f64 } fn main() {}",
        &[
            "struct Point  size=16 align=8",
            "@0    x: f64",
            "@8    y: f64",
        ],
    );
}

#[test]
fn mixed_fields_get_c_style_padding() {
    // u8 at 0, then i64 must align to 8, so a 7-byte hole; total 16.
    has(
        "struct Mixed { a: u8, b: i64 } fn main() {}",
        &[
            "struct Mixed  size=16 align=8",
            "@0    a: u8  (size=1 align=1)",
            "@8    b: i64  (size=8 align=8)",
        ],
    );
}

#[test]
fn trailing_small_field_pads_to_struct_align() {
    // i64 then u8: u8 at offset 8, struct rounds up to 16 (align 8).
    has(
        "struct Tail { a: i64, b: u8 } fn main() {}",
        &[
            "struct Tail  size=16 align=8",
            "@0    a: i64",
            "@8    b: u8",
        ],
    );
}

#[test]
fn heap_field_is_pointer_sized() {
    has(
        "struct Holder { name: str, items: List<i32> } fn main() {}",
        &[
            "struct Holder  size=16 align=8",
            "@0    name: str  (size=8 align=8)",
            "@8    items: List<i32>  (size=8 align=8)",
        ],
    );
}

#[test]
fn fixed_array_field_layout() {
    has(
        "struct Buf { data: [u8; 4] } fn main() {}",
        &[
            "struct Buf  size=4 align=1",
            "@0    data: [u8; 4]  (size=4 align=1)",
        ],
    );
}

#[test]
fn unit_only_enum_is_one_byte() {
    has(
        "enum Dir { North, South, East, West } fn main() {}",
        &["enum Dir  size=1 align=1 tag=1B payload@1", "North (unit)"],
    );
}

#[test]
fn enum_with_data_is_a_tagged_union() {
    // Largest payload is three f64 (24 bytes) at offset 8 → size 32, align 8.
    has(
        "enum Shape { Circle(f64), Rect { width: f64, height: f64 }, Triangle(f64, f64, f64) } fn main() {}",
        &[
            "enum Shape  size=32 align=8 tag=1B payload@8",
            "@8    0: f64",
            "@8    width: f64",
            "@16   height: f64",
            "@24   2: f64",
        ],
    );
}

#[test]
fn nested_struct_field_is_inlined() {
    // A struct field is laid out by value, not by pointer.
    has(
        "struct Point { x: f64, y: f64 }\n\
         struct Line { a: Point, b: Point }\n\
         fn main() {}",
        &[
            "struct Line  size=32 align=8",
            "@0    a: Point  (size=16 align=8)",
            "@16   b: Point  (size=16 align=8)",
        ],
    );
}

#[test]
fn generic_struct_is_skipped() {
    has(
        "struct Pair<T> { a: T, b: T } fn main() {}",
        &["(skipped struct Pair (generic))"],
    );
}

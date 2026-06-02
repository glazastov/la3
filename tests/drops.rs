//! Phase 1.6.5 — drop classification (reference Section 11, deterministic
//! destruction). A type "needs drop" iff it (transitively) owns a heap resource
//! that must be released. The front-end exposes this through `la3 layout`
//! (`drop=yes/no` per struct/enum); the MIR ownership-lowering pass (Phase 3.5)
//! consumes it to decide *where* to insert the drops the borrow check proved
//! safe. These cases pin the classification down per the contract.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Run `la3 layout` and return the `drop=…` verdict for `decl` (e.g. `Point`).
fn drop_of(src: &str, decl: &str) -> bool {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_drops_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["layout", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    let dump = String::from_utf8_lossy(&out.stdout);
    let line = dump
        .lines()
        .find(|l| {
            (l.starts_with("struct ") || l.starts_with("enum "))
                && l.split_whitespace().nth(1) == Some(decl)
        })
        .unwrap_or_else(|| panic!("no layout line for `{}` in:\n{}", decl, dump));
    if line.contains("drop=yes") {
        true
    } else if line.contains("drop=no") {
        false
    } else {
        panic!("no drop verdict on line: {}", line)
    }
}

// ---------------------------------------------------------------------------
// Scalars and references own nothing → no drop
// ---------------------------------------------------------------------------

#[test]
fn all_scalar_struct_needs_no_drop() {
    assert!(!drop_of(
        "struct P { x: f64, y: i32, ok: bool }\nfn main() { io.println(0) }",
        "P"
    ));
}

#[test]
fn unit_enum_needs_no_drop() {
    assert!(!drop_of(
        "enum Dir { North, South, East, West }\nfn main() { io.println(0) }",
        "Dir"
    ));
}

#[test]
fn enum_with_only_scalar_payloads_needs_no_drop() {
    assert!(!drop_of(
        "enum Shape { Circle(f64), Rect { w: f64, h: f64 } }\nfn main() { io.println(0) }",
        "Shape"
    ));
}

// ---------------------------------------------------------------------------
// Heap-owning fields → drop (transitively)
// ---------------------------------------------------------------------------

#[test]
fn struct_with_a_string_field_needs_drop() {
    assert!(drop_of(
        "struct U { name: str, age: i32 }\nfn main() { io.println(0) }",
        "U"
    ));
}

#[test]
fn struct_with_a_list_field_needs_drop() {
    assert!(drop_of(
        "struct Buf { data: List<u8>, len: i32 }\nfn main() { io.println(0) }",
        "Buf"
    ));
}

#[test]
fn drop_is_transitive_through_a_nested_struct() {
    // `Outer` owns no heap directly, but its `inner: Inner` field does.
    let src = "struct Inner { s: str }\nstruct Outer { inner: Inner, n: i32 }\nfn main() { io.println(0) }";
    assert!(drop_of(src, "Inner"), "Inner owns a `str`");
    assert!(drop_of(src, "Outer"), "Outer transitively owns Inner's `str`");
}

#[test]
fn enum_with_a_heap_payload_needs_drop() {
    assert!(drop_of(
        "enum Msg { Ping, Text(str) }\nfn main() { io.println(0) }",
        "Msg"
    ));
}

#[test]
fn array_of_heap_elements_needs_drop() {
    assert!(drop_of(
        "struct Names { all: [str; 3] }\nfn main() { io.println(0) }",
        "Names"
    ));
}

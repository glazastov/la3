//! Phase 2.3 — HIR lowering (typed, `BindingId`-based, no re-inference).
//!
//! Drives `la3 hir`, whose dump renders the lowered tree with each node's
//! embedded `Ty` (`… : ty`) and every binding/use as a `BindingId` (`#n` /
//! `Local(#n)`). The point of HIR: types come straight from the type table and
//! locals are ids, so the back-end never re-infers types or reasons about names.
//!
//! Lowering also depends on a **load-bearing invariant**: it allocates binding
//! ids with a sequential counter that must mirror name resolution's allocation
//! order exactly (guarded by a `debug_assert` in `hir::lower`). Running these —
//! and the full example suite via `la3 hir` — exercises that alignment.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn hir(src: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_hir_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["hir", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "hir failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn params_and_uses_become_binding_ids() {
    // `a`/`b` are bindings `#0`/`#1`; their uses are `Local(#0)`/`Local(#1)`.
    let dump = hir("fn add(a: i32, b: i32) -> i32 { a + b }");
    assert!(dump.contains("fn add(a#0: i32, b#1: i32) -> i32"), "{}", dump);
    assert!(dump.contains("Local(#0)"), "{}", dump);
    assert!(dump.contains("Local(#1)"), "{}", dump);
}

#[test]
fn expressions_carry_their_type() {
    // Types are embedded from the type table, not re-derived.
    let dump = hir("fn main() { let x = 1 + 2; let b = x < 3; io.println(b) }");
    assert!(dump.contains("Binary(Add) : i32"), "{}", dump);
    assert!(dump.contains("Binary(Lt) : bool"), "{}", dump);
    // `let x` records the binding type from its value.
    assert!(dump.contains("let #0 : i32"), "{}", dump);
}

#[test]
fn globals_are_not_locals() {
    // A call to a free function resolves the callee to a global, not a `Local`.
    let dump = hir("fn f() -> i32 { 1 }\nfn main() { io.println(f()) }");
    assert!(dump.contains("Global(f)"), "{}", dump);
    assert!(dump.contains("Global(io)"), "{}", dump);
    // No local was introduced, so there is no `Local(` anywhere.
    assert!(!dump.contains("Local("), "unexpected local:\n{}", dump);
}

#[test]
fn shadowing_uses_distinct_ids() {
    // The two `x` bindings get distinct ids; each use resolves to the one in scope.
    let dump = hir("fn main() { let x = 1; let y = x; let x = 2; io.println(x + y) }");
    assert!(dump.contains("let #0 : i32"), "first x is #0:\n{}", dump);
    assert!(dump.contains("let #2 : i32"), "shadowing x is #2:\n{}", dump);
    // `let y = x` reads the first x (#0); the final `x + y` reads #2 and #1.
    assert!(dump.contains("Local(#0)"), "{}", dump);
    assert!(dump.contains("Local(#2)"), "{}", dump);
}

#[test]
fn for_loop_binding_and_self_resolve() {
    let dump = hir("fn main() { for i in 0..3 { io.println(i) } }");
    assert!(dump.contains("pat #0"), "loop var is a binding:\n{}", dump);
    assert!(dump.contains("Local(#0) : i32"), "use resolves to it:\n{}", dump);
    assert!(dump.contains("Range(inclusive=false) : Range<i32>"), "{}", dump);
}

#[test]
fn method_self_is_a_local_binding() {
    // `&self` becomes binding `#0`; `self.x` reads `Local(#0)`. The method is
    // attached to its owner type.
    let src = "struct P { x: i32 }\n\
               impl P { fn get(&self) -> i32 { self.x } }\n\
               fn main() { let p = P { x: 5 }; io.println(p.get()) }";
    let dump = hir(src);
    assert!(dump.contains("fn P::get(self#0:"), "method owner + self:\n{}", dump);
    assert!(dump.contains("Field(x) : i32"), "{}", dump);
    assert!(dump.contains("Local(#0)"), "self use:\n{}", dump);
}

#[test]
fn struct_and_enum_decls_lower_with_field_types() {
    let src = "struct Pt { x: f64, y: f64 }\n\
               enum Shape { Circle(f64), Rect { w: f64, h: f64 } }\n\
               fn main() {}";
    let dump = hir(src);
    assert!(dump.contains("struct Pt"), "{}", dump);
    assert!(dump.contains("x: f64"), "{}", dump);
    assert!(dump.contains("enum Shape"), "{}", dump);
    assert!(dump.contains("Circle(f64)"), "{}", dump);
    assert!(dump.contains("Rect { w: f64, h: f64 }"), "{}", dump);
}

#[test]
fn match_arms_bind_and_resolve() {
    let src = "fn main() { let o = Some(3); match o { Some(n) => io.println(n), None => {} } }";
    let dump = hir(src);
    assert!(dump.contains("Match :"), "{}", dump);
    // The arm binds `n` (some `#id`) and the body reads it as a local.
    assert!(dump.contains("arm Some(#"), "variant pattern binds:\n{}", dump);
}

#[test]
fn fstring_is_retained_as_sugar_for_2_4() {
    // 2.3 lowers structurally: the f-string is kept (desugaring is 2.4).
    let dump = hir("fn main() { let n = 7; io.println(f\"n = {n}\") }");
    assert!(dump.contains("FStr : str"), "{}", dump);
    assert!(dump.contains("lit \"n = \""), "{}", dump);
}

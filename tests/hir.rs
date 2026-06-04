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

// ---------------------------------------------------------------------------
// Phase 2.4 — desugarings (HIR carries no surface sugar)
// ---------------------------------------------------------------------------

#[test]
fn fstring_desugars_to_format_and_concat() {
    // `f"n = {n}"` → `"n = " + format(n)`; no `FStr` survives.
    let dump = hir("fn main() { let n = 7; io.println(f\"n = {n}\") }");
    assert!(!dump.contains("FStr"), "f-string should be gone:\n{}", dump);
    assert!(dump.contains("Binary(Add) : str"), "concat:\n{}", dump);
    assert!(dump.contains("Format : str"), "format primitive:\n{}", dump);
    assert!(dump.contains("Str(\"n = \")"), "literal segment:\n{}", dump);
}

#[test]
fn fstring_with_spec_keeps_the_spec_on_format() {
    let dump = hir("fn main() { let x = 5; io.println(f\"{x:>3}\") }");
    assert!(dump.contains("Format(:>3)"), "spec retained:\n{}", dump);
}

#[test]
fn coalesce_desugars_to_a_nil_match() {
    // `a ?? b` → `match a { nil => b, t => t }`; no `Coalesce` survives.
    let dump = hir("fn main() { let a = nil; let b = a ?? \"x\"; io.println(b) }");
    assert!(!dump.contains("Coalesce"), "?? should be gone:\n{}", dump);
    assert!(dump.contains("Match :"), "{}", dump);
    assert!(dump.contains("arm nil"), "nil arm:\n{}", dump);
    assert!(dump.contains("Str(\"x\")"), "fallback in nil arm:\n{}", dump);
}

#[test]
fn optional_chain_desugars_to_a_nil_match() {
    // `u?.name` → `match u { nil => nil, t => t.name }`.
    let src = "struct U { name: str }\n\
               fn f(u: U | nil) -> str | nil { u?.name }\n\
               fn main() {}";
    let dump = hir(src);
    assert!(dump.contains("arm nil"), "nil arm:\n{}", dump);
    assert!(dump.contains("Field(name)"), "plain field in non-nil arm:\n{}", dump);
    // No `?.` marker remains anywhere.
    assert!(!dump.contains("?."), "optional marker gone:\n{}", dump);
}

#[test]
fn try_on_result_desugars_to_ok_err_match_with_return() {
    // `e?` on a Result → `match e { Ok(v) => v, Err(x) => return Err(x) }`.
    let src = "fn run() -> Result<str> { let t = fs.read(\"p\")?; Ok(t) }\nfn main() {}";
    let dump = hir(src);
    assert!(dump.contains("arm Ok(#"), "Ok arm binds:\n{}", dump);
    assert!(dump.contains("arm Err(#"), "Err arm binds:\n{}", dump);
    assert!(dump.contains("Global(Err)"), "reconstructs Err:\n{}", dump);
    // The Err arm early-returns.
    assert!(dump.contains("return"), "early return:\n{}", dump);
}

#[test]
fn compound_assign_desugars_to_plain_assign_plus_binary() {
    // `n += 5` → `n = n + 5`.
    let dump = hir("fn main() { let mut n = 0; n += 5 }");
    assert!(dump.contains("Assign :"), "{}", dump);
    assert!(dump.contains("Binary(Add) : i32"), "rebuilt operation:\n{}", dump);
    // The target appears on both sides (place and operand).
    let locals = dump.matches("Local(#0)").count();
    assert!(locals >= 2, "target used as place and operand:\n{}", dump);
}

#[test]
fn while_let_desugars_to_loop_match_break() {
    // `while let Some(x) = e { … }` → `loop { match e { Some(x) => …, _ => break } }`.
    let dump = hir("fn main() { let mut xs = [1,2,3]; while let Some(x) = xs.pop() { io.println(x) } }");
    assert!(!dump.contains("WhileLet"), "while-let should be gone:\n{}", dump);
    assert!(dump.contains("Loop :"), "{}", dump);
    assert!(dump.contains("arm Some(#"), "match arm binds:\n{}", dump);
    assert!(dump.contains("arm _"), "wildcard arm:\n{}", dump);
    assert!(dump.contains("break"), "break in wildcard arm:\n{}", dump);
}

#[test]
fn desugaring_temporaries_get_fresh_ids_past_real_bindings() {
    // `a` is the only real binding (#0); the `??` temporary must not reuse it.
    let dump = hir("fn main() { let a = nil; let b = a ?? \"x\"; io.println(b) }");
    // The synthetic binding in the catch-all arm has an id >= the real count.
    // Real bindings here: a(#0), b(#1). The temp should be #2 or higher.
    assert!(
        dump.contains("arm #2") || dump.contains("arm #3"),
        "fresh synthetic id past reals:\n{}",
        dump
    );
}

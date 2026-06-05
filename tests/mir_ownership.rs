//! Phase 3.5 — ownership lowering (move threading + drop insertion).
//!
//! These tests pin the MIR-level ownership facts `la3 mir` now emits: owned
//! (`needs_drop`) bindings get a `drop` at their scope exit (reverse declaration
//! order, on every path), a value read in a consuming position becomes a `move`
//! and is then *not* dropped (its new owner is responsible), and built-ins /
//! `&self` methods / references borrow rather than move. The interpreter is the
//! oracle and is untouched; there is no codegen yet, so these assert the emitted
//! MIR shape rather than runtime behaviour.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn mir(src: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_own_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["mir", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "mir failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dump = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !dump.contains("invalid MIR"),
        "produced invalid MIR:\n{}",
        dump
    );
    dump
}

#[test]
fn owned_binding_is_dropped_at_scope_end() {
    let dump = mir("fn f() { let s = \"hi\" }");
    assert!(dump.contains("drop(_1)"), "owned `str` dropped:\n{}", dump);
}

#[test]
fn copy_binding_is_not_dropped() {
    let dump = mir("fn f() { let n = 5 }");
    assert!(
        !dump.contains("drop("),
        "a Copy value needs no drop:\n{}",
        dump
    );
}

#[test]
fn move_into_a_binding_retires_the_source() {
    // `let t = s` moves `s`; `s` is then not dropped, only the new owner is.
    let dump = mir("fn g(s: str) -> str { let t = s\n t }");
    assert!(
        dump.contains("_2 = move _1"),
        "source moved into the binding:\n{}",
        dump
    );
    assert!(
        dump.contains("_0 = move _2"),
        "binding moved into the return:\n{}",
        dump
    );
    assert!(
        !dump.contains("drop("),
        "moved-out values are not dropped:\n{}",
        dump
    );
}

#[test]
fn by_value_user_argument_is_moved_and_the_callee_drops_its_param() {
    let dump = mir("fn take(s: str) {}\nfn f() { let s = \"x\"\n take(s) }");
    assert!(
        dump.contains("call take(move _1)"),
        "argument moved into the call:\n{}",
        dump
    );
    // The callee owns its by-value parameter and drops it at exit.
    assert!(dump.contains("fn take(_1: str)"), "{}", dump);
    assert!(
        dump.contains("drop(_1)"),
        "callee drops its owned param:\n{}",
        dump
    );
}

#[test]
fn self_method_consumes_the_receiver() {
    let dump = mir("struct B { s: str }\n\
         impl B { fn consume(self) -> str { self.s } }\n\
         fn f(b: B) -> str { b.consume() }");
    assert!(
        dump.contains("call B::consume(move _1)"),
        "receiver moved:\n{}",
        dump
    );
}

#[test]
fn builtin_and_shared_methods_borrow_the_receiver() {
    // `str::len` is a built-in `&self`-style method: it borrows, so `s` is still
    // owned afterward and dropped at scope end.
    let dump = mir("fn f(s: str) { let n = s.len() }");
    assert!(
        dump.contains("call str::len(copy _1)"),
        "receiver borrowed:\n{}",
        dump
    );
    assert!(
        dump.contains("drop(_1)"),
        "borrowed receiver still dropped:\n{}",
        dump
    );
}

#[test]
fn a_reference_is_a_borrow_not_a_move() {
    let dump = mir("fn f(s: str) { let r = &s }");
    assert!(dump.contains("_2 = &_1"), "takes a reference:\n{}", dump);
    assert!(
        dump.contains("drop(_1)"),
        "the borrowed value is still owned/dropped:\n{}",
        dump
    );
}

#[test]
fn drops_run_in_reverse_declaration_order() {
    let dump = mir("fn f() { let a = \"a\"\n let b = \"b\" }");
    let da = dump.find("drop(_1)").expect("a is dropped");
    let db = dump.find("drop(_2)").expect("b is dropped");
    assert!(db < da, "later binding `b` drops before `a`:\n{}", dump);
}

#[test]
fn early_return_drops_owned_locals_on_both_paths() {
    // The return path and the fall-through are mutually exclusive, so each drops
    // the live owned locals exactly once (reverse order).
    let dump = mir("fn f(c: bool) { let a = \"a\"\n let b = \"b\"\n if c { return } }");
    let drops = dump.matches("drop(_3)").count();
    assert!(
        drops >= 2,
        "`b` is dropped on both the return and fall-through paths:\n{}",
        dump
    );
    assert!(dump.contains("drop(_2)"), "`a` is dropped too:\n{}", dump);
}

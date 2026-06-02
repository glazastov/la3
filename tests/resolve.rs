//! Phase 2.2 — name resolution → `BindingId`s.
//!
//! Drives `la3 resolve`, whose dump lists every value binding (`#id name`) and
//! every local use resolved to its binding (`line:col name -> #id`). The point
//! is that **shadowing is resolved here, once**: two `let x` get distinct ids and
//! each use points to the binding actually in scope, so downstream passes never
//! reason about names.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn resolve(src: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_resolve_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["resolve", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "resolve failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn distinct_bindings_get_distinct_ids() {
    // Two `let x` are two bindings; `y` is a third.
    let dump = resolve("fn main() { let x = 1; let y = 2; let z = x + y; io.println(z) }");
    assert!(dump.contains("#0   x"), "dump:\n{}", dump);
    assert!(dump.contains("#1   y"), "dump:\n{}", dump);
    assert!(dump.contains("#2   z"), "dump:\n{}", dump);
}

#[test]
fn shadowing_resolves_each_use_to_the_binding_in_scope() {
    // `let y = x` sees the first `x` (#0); after `let x = 2` (#2), the use sees #2.
    let dump = resolve("fn main() { let x = 1; let y = x; let x = 2; io.println(x); io.println(y) }");
    assert!(dump.contains("x -> #0"), "early x should resolve to #0:\n{}", dump);
    assert!(dump.contains("x -> #2"), "shadowed x should resolve to #2:\n{}", dump);
    assert!(dump.contains("y -> #1"), "y should resolve to #1:\n{}", dump);
}

#[test]
fn inner_scope_binding_does_not_escape() {
    // The inner `x` (#1) is used inside the block; the outer use sees the outer x (#0).
    let dump = resolve(
        "fn main() { let x = 1; { let x = 2; io.println(x) } io.println(x) }",
    );
    assert!(dump.contains("x -> #0"), "outer use should be #0:\n{}", dump);
    assert!(dump.contains("x -> #1"), "inner use should be #1:\n{}", dump);
}

#[test]
fn parameters_are_bindings_and_uses_resolve_to_them() {
    let dump = resolve("fn add(a: i32, b: i32) -> i32 { a + b }\nfn main() { io.println(add(1, 2)) }");
    assert!(dump.contains("#0   a"), "dump:\n{}", dump);
    assert!(dump.contains("#1   b"), "dump:\n{}", dump);
    assert!(dump.contains("a -> #0") && dump.contains("b -> #1"), "dump:\n{}", dump);
}

#[test]
fn loop_pattern_binding_is_resolved() {
    let dump = resolve("fn main() { for i in 0..3 { io.println(i) } }");
    // `i` is a binding and the use inside the body resolves to it.
    assert!(dump.contains("i -> #0"), "loop var use should resolve:\n{}", dump);
}

#[test]
fn globals_and_builtins_are_not_local_bindings() {
    // `io.println` / `add` are global/builtin — they produce no `-> #` use lines.
    let dump = resolve("fn add(a: i32) -> i32 { a }\nfn main() { io.println(add(5)) }");
    // The only resolved local use is `a` inside `add`.
    let use_lines = dump
        .lines()
        .skip_while(|l| !l.starts_with("uses:"))
        .filter(|l| l.contains("-> #"))
        .count();
    assert_eq!(use_lines, 1, "only `a` should resolve to a local:\n{}", dump);
}

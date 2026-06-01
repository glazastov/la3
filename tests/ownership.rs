//! Phase 1.6.1 — move semantics & use-after-move (reference Section 11).
//!
//! Driven through `la3 check`. A *move* (`let y = x` / `x = y` of a non-`Copy`
//! binding) ends the source binding's ownership; reading it afterward is an
//! error. `Copy` values (scalars, references, …) may be reused freely. Argument
//! and receiver moves are out of scope until 1.6.2, so passing a value to a
//! function or calling a method on it must NOT be treated as a move yet.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn check(src: &str) -> (String, bool) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_own_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["check", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.success())
}

fn ok(src: &str) {
    let (text, success) = check(src);
    assert!(success, "expected clean check, got:\n{}", text);
}

fn rejects(src: &str, needle: &str) {
    let (text, success) = check(src);
    assert!(
        !success,
        "expected a move error, but check passed:\n{}",
        text
    );
    assert!(
        text.contains(needle),
        "expected error to mention {:?}, got:\n{}",
        needle,
        text
    );
}

// ---------------------------------------------------------------------------
// Moves and use-after-move
// ---------------------------------------------------------------------------

#[test]
fn use_after_move_of_a_list_is_rejected() {
    rejects(
        "fn main() { let a = [1, 2, 3]; let b = a; io.println(b); io.println(a) }",
        "use of moved value `a`",
    );
}

#[test]
fn use_after_move_of_a_string_is_rejected() {
    rejects(
        "fn main() { let s = \"hi\".to_upper(); let t = s; io.println(t); io.println(s) }",
        "use of moved value `s`",
    );
}

#[test]
fn move_via_assignment_is_tracked() {
    rejects(
        "fn main() { let a = [1, 2, 3]; let mut b = [0]; b = a; io.println(b); io.println(a) }",
        "use of moved value `a`",
    );
}

#[test]
fn moving_the_value_into_the_new_binding_is_fine() {
    // Using the destination after the move is always allowed.
    ok("fn main() { let a = [1, 2, 3]; let b = a; io.println(b) }");
}

// ---------------------------------------------------------------------------
// Copy types are exempt
// ---------------------------------------------------------------------------

#[test]
fn copy_scalars_can_be_reused() {
    ok("fn main() { let a = 5; let b = a; io.println(a); io.println(b) }");
}

#[test]
fn re_binding_restores_ownership() {
    // After `a` is moved, a fresh `let a = ...` makes it usable again.
    ok("fn main() { let a = [1, 2, 3]; let b = a; let a = [4, 5]; io.println(a); io.println(b) }");
}

// ---------------------------------------------------------------------------
// Flow sensitivity
// ---------------------------------------------------------------------------

#[test]
fn conditional_move_taints_the_value_afterward() {
    // Moved on one branch ⇒ moved after the `if` (union rule, as in Rust).
    rejects(
        "fn main() { let a = [1, 2, 3]; if true { let b = a; io.println(b) }; io.println(a) }",
        "use of moved value `a`",
    );
}

#[test]
fn move_in_a_loop_then_reuse_is_rejected() {
    // The value is moved on the first iteration and read on the next.
    rejects(
        "fn main() { let a = [1, 2, 3]; for _i in 0..3 { let b = a; io.println(b) } }",
        "use of moved value `a`",
    );
}

// ---------------------------------------------------------------------------
// Out of scope for 1.6.1 (must NOT be flagged as moves yet)
// ---------------------------------------------------------------------------

#[test]
fn passing_to_a_method_does_not_move_the_receiver() {
    // `xs.map(..)` borrows the receiver; reusing `xs` is fine.
    ok(
        "fn main() { let xs = [1, 2, 3]; let ys = xs.map(|x| x * 2); io.println(xs); io.println(ys) }",
    );
}

#[test]
fn reusing_an_argument_in_the_same_call_is_fine() {
    // The word_count idiom: `m.get(k)` borrows `k`, so `m[k]` after is fine.
    ok(
        "fn main() { let mut m: Map<str, i64> = {}; let k = \"a\"; m[k] = m.get(k).unwrap_or(0) + 1; io.println(m.len()) }",
    );
}

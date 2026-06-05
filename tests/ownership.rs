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
// Built-ins always borrow (never move their args/receiver)
// ---------------------------------------------------------------------------

#[test]
fn passing_to_a_builtin_method_does_not_move_the_receiver() {
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

// ---------------------------------------------------------------------------
// 1.6.2 — argument & receiver moves (user functions/methods)
// ---------------------------------------------------------------------------

#[test]
fn by_value_argument_to_a_user_fn_moves() {
    rejects(
        "fn take(x: List<i32>) -> i32 { x.len() as i32 }\n\
         fn main() { let a = [1, 2, 3]; let n = take(a); io.println(a); io.println(n) }",
        "use of moved value `a`",
    );
}

#[test]
fn borrowed_argument_does_not_move() {
    ok("fn take(x: &List<i32>) -> i32 { x.len() as i32 }\n\
         fn main() { let a = [1, 2, 3]; let n = take(&a); io.println(a); io.println(n) }");
}

#[test]
fn consuming_method_moves_the_receiver() {
    rejects(
        "struct B { v: List<i32> }\n\
         impl B { fn eat(self) -> i32 { self.v.len() as i32 } }\n\
         fn main() { let b = B { v: [1, 2] }; let n = b.eat(); io.println(b.v); io.println(n) }",
        "use of moved value `b`",
    );
}

#[test]
fn ref_self_method_does_not_move_the_receiver() {
    ok("struct B { v: List<i32> }\n\
         impl B { fn size(&self) -> i32 { self.v.len() as i32 } }\n\
         fn main() { let b = B { v: [1, 2] }; let n = b.size(); let m = b.size(); io.println(n + m) }");
}

#[test]
fn passing_a_struct_by_value_then_using_it_is_rejected() {
    // The http_server bug: `route(req)` consumes `req`, then `req.path` is read.
    rejects(
        "struct R { path: str }\n\
         fn route(r: R) -> str { r.path }\n\
         fn main() { let req = R { path: \"/\" }; let p = route(req); io.println(p); io.println(req.path) }",
        "use of moved value `req`",
    );
}

// ---------------------------------------------------------------------------
// 1.6.3 — `move`-closure captures
// ---------------------------------------------------------------------------

#[test]
fn move_closure_captures_are_moved() {
    rejects(
        "fn main() { let a = [1, 2, 3]; let f = move || a.len(); io.println(f()); io.println(a) }",
        "use of moved value `a`",
    );
}

#[test]
fn non_move_closure_borrows_its_captures() {
    ok("fn main() { let a = [1, 2, 3]; let f = || a.len(); io.println(f()); io.println(a) }");
}

#[test]
fn move_closure_capturing_a_copy_value_is_fine() {
    // An `i32` is `Copy`, so a `move` closure copies it — the original stays usable.
    ok("fn main() { let n = 7; let f = move || n + 1; io.println(f()); io.println(n) }");
}

// ---------------------------------------------------------------------------
// 1.6.4 — borrow regions: `&mut` exclusivity & lifetimes
// ---------------------------------------------------------------------------

#[test]
fn using_a_value_while_mutably_borrowed_is_rejected() {
    rejects(
        "fn main() { let mut v = [1, 2, 3]; let r = &mut v; v.push(4); io.println(r) }",
        "while it is mutably borrowed by `r`",
    );
}

#[test]
fn reassigning_a_value_while_shared_borrowed_is_rejected() {
    // A shared borrow forbids writes to the borrowed place (here, reassignment).
    rejects(
        "fn main() { let mut v = [1, 2, 3]; let r = &v; v = [4, 5]; io.println(r) }",
        "while it is borrowed by `r`",
    );
}

#[test]
fn mutating_via_a_method_while_shared_borrowed_is_rejected() {
    // `push` mutates its receiver, so it conflicts with a live shared borrow.
    rejects(
        "fn main() { let mut v = [1, 2, 3]; let r = &v; v.push(4); io.println(r) }",
        "cannot mutate `v`",
    );
}

#[test]
fn read_only_method_while_shared_borrowed_is_fine() {
    // `len`/`&self` methods only read, so they coexist with a shared borrow.
    ok("fn main() { let v = [1, 2, 3]; let r = &v; io.println(v.len()); io.println(r.len()) }");
}

#[test]
fn mut_self_method_while_borrowed_is_rejected() {
    // A user `&mut self` method mutates, so it conflicts with a live borrow.
    rejects(
        "struct B { v: List<i32> }\n\
         impl B { fn add(&mut self, x: i32) { self.v.push(x) } fn size(&self) -> i32 { self.v.len() as i32 } }\n\
         fn main() { let mut b = B { v: [1] }; let r = &b; b.add(2); io.println(r.size()) }",
        "cannot mutate `b`",
    );
}

#[test]
fn reading_a_value_while_shared_borrowed_is_fine() {
    // A shared borrow permits other reads (`&` xor `&mut`).
    ok("fn main() { let v = [1, 2, 3]; let r = &v; io.println(v.len()); io.println(r.len()) }");
}

#[test]
fn two_mutable_borrows_at_once_are_rejected() {
    rejects(
        "fn main() { let mut v = [1, 2, 3]; let a = &mut v; let b = &mut v; io.println(a); io.println(b) }",
        "aliasing xor mutability",
    );
}

#[test]
fn passing_a_mut_ref_directly_does_not_lock_the_value() {
    // A `&mut n` created as a call argument is a within-call borrow; after the
    // call the value is free again (the memory.la3 idiom).
    ok(
        "fn bump(x: &mut i32) { *x += 1 }\nfn main() { let mut n = 1; bump(&mut n); io.println(n) }",
    );
}

#[test]
fn returning_a_reference_to_a_local_is_rejected() {
    rejects(
        "fn dangle() -> &i32 { let x = 5; &x }\nfn main() { io.println(0) }",
        "reference to local `x`",
    );
}

#[test]
fn a_borrow_ends_with_its_block() {
    // `r` is confined to the inner block, so `v` is free again afterward.
    ok(
        "fn main() { let mut v = [1, 2, 3]; { let r = &mut v; io.println(r.len()) } v.push(4); io.println(v.len()) }",
    );
}

// ---------------------------------------------------------------------------
// 1.6.4 — field-granular borrows (disjoint fields don't conflict)
// ---------------------------------------------------------------------------

#[test]
fn borrowing_one_field_leaves_other_fields_free() {
    // `&u.name` must not lock `u.age` — they are disjoint memory.
    ok("struct U { name: str, age: i32 }\n\
         fn main() { let mut u = U { name: \"a\", age: 1 }; let r = &u.name; u.age = 30; io.println(r) }");
}

#[test]
fn borrowing_a_field_locks_that_same_field() {
    rejects(
        "struct U { name: str, age: i32 }\n\
         fn main() { let mut u = U { name: \"a\", age: 1 }; let r = &u.name; u.name = \"b\"; io.println(r) }",
        "cannot mutate `u.name`",
    );
}

#[test]
fn borrowing_the_whole_value_locks_every_field() {
    // A borrow of `u` (no projection) covers all of its fields: mutating `u.age`
    // conflicts, and the message names the held place (`u`).
    rejects(
        "struct U { name: str, age: i32 }\n\
         fn main() { let mut u = U { name: \"a\", age: 1 }; let r = &u; u.age = 30; io.println(r.age) }",
        "cannot mutate `u.age` while `u` is borrowed by `r`",
    );
}

// ---------------------------------------------------------------------------
// Deferred to MIR (Phase 3.7) — NLL & reborrows.
//
// These assert the *correct* (Rust-accurate) behaviour: the code below is
// memory-safe and a precise borrow checker accepts it. The current AST pass is a
// sound lexical over-approximation, so it (wrongly) rejects them today — hence
// `#[ignore]`. Run `cargo test -- --ignored` to watch them fail now; when the
// MIR-based borrow check lands (Phase 3.7), delete the `#[ignore]` and they go
// green. They must NEVER be weakened to pass early.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs NLL (borrow ends at last use) — MIR borrow-check, Phase 3.7"]
fn nll_shared_borrow_dead_before_mutation_is_ok() {
    // `r`'s last use is `r.len()`; afterwards `v` is free to mutate.
    ok(
        "fn main() { let mut v = [1, 2, 3]; let r = &v; io.println(r.len()); v.push(4); io.println(v.len()) }",
    );
}

#[test]
#[ignore = "needs NLL (borrow ends at last use) — MIR borrow-check, Phase 3.7"]
fn nll_sequential_mut_borrows_are_ok() {
    // `a` is dead after `a.push(1)`, so taking `b = &mut v` next is safe.
    ok(
        "fn main() { let mut v = [1, 2, 3]; let a = &mut v; a.push(1); let b = &mut v; b.push(2); io.println(v.len()) }",
    );
}

#[test]
#[ignore = "needs reborrow tracking (`&mut *r`) — MIR borrow-check, Phase 3.7"]
fn reborrow_releases_the_parent_after_use() {
    // `r2` reborrows `x` through `r1`; once `r2` is done, `r1` is usable again.
    ok(
        "fn main() { let mut x = 5; let r1 = &mut x; let r2 = &mut *r1; *r2 += 1; *r1 += 1; io.println(x) }",
    );
}

#[test]
fn distinct_indices_are_conservatively_treated_as_one() {
    // Indices are dynamic, so `&a[0]` conservatively locks the whole array's
    // elements (faithful to Rust's borrow checker — index disjointness needs an
    // explicit API). A write to another index is therefore still rejected.
    rejects(
        "fn main() { let mut a: [i32; 3] = [1, 2, 3]; let r = &a[0]; a[1] = 9; io.println(r) }",
        "while it is borrowed by `r`",
    );
}

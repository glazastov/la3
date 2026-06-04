//! Phase 3.2 — HIR → MIR core lowering.
//!
//! Drives `la3 mir`, which lowers every function the core supports into a
//! Rust-MIR-flavoured CFG and lists the ones it skips (match/closures/heap
//! literals/…) with a reason. The milestone is that `fib` lowers completely;
//! these tests pin the CFG shapes (if/loop/while/for, calls, break-with-value)
//! and the honest deferral of `match` to 3.3. Every emitted function has already
//! passed `MirFn::validate` inside the lowering (a failure would show up as a
//! `skipped — invalid MIR` line, which several tests assert never happens).

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn mir(src: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_mir_{}_{}.la3", std::process::id(), n));
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn fib_lowers_completely_and_validates() {
    let src = "fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\n\
               fn main() { for i in 0..10 { io.println(fib(i)) } }";
    let dump = mir(src);
    assert!(dump.contains("fn fib(_1: i64) -> i64"), "{}", dump);
    assert!(dump.contains("fn main() -> ()"), "{}", dump);
    assert!(!dump.contains("skipped"), "fib/main must fully lower:\n{}", dump);
    assert!(!dump.contains("invalid MIR"), "must validate:\n{}", dump);
    // The if-expression branches and the recursive calls are present.
    assert!(dump.contains("Lt(copy _1, const 2"), "{}", dump);
    assert!(dump.contains("call fib("), "{}", dump);
    assert!(dump.contains("return"), "{}", dump);
}

#[test]
fn straight_line_arithmetic() {
    let dump = mir("fn add(a: i32, b: i32) -> i32 { a + b }");
    assert!(dump.contains("fn add(_1: i32, _2: i32) -> i32"), "{}", dump);
    assert!(dump.contains("Add(copy _1, copy _2)"), "{}", dump);
    // Result threaded into the return slot _0, then `return`.
    assert!(dump.contains("_0 = "), "{}", dump);
    assert!(dump.contains("return"), "{}", dump);
}

#[test]
fn if_expression_threads_a_result_value() {
    let dump = mir("fn pick(b: bool) -> i32 { if b { 1 } else { 2 } }");
    assert!(dump.contains("if copy"), "branch on the condition:\n{}", dump);
    // Both arms assign the same result temp, which becomes _0.
    assert!(dump.contains("const 1_i32"), "{}", dump);
    assert!(dump.contains("const 2_i32"), "{}", dump);
    assert!(dump.contains("goto -> "), "arms jump to the join:\n{}", dump);
}

#[test]
fn for_over_exclusive_range_is_a_counter_loop() {
    let dump = mir("fn main() { for i in 0..3 { io.println(i) } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    // header compares with `<`, increment adds 1.
    assert!(dump.contains("Lt(copy"), "exclusive uses Lt:\n{}", dump);
    assert!(dump.contains("Add(copy") && dump.contains("const 1"), "increment:\n{}", dump);
}

#[test]
fn for_over_inclusive_range_uses_le() {
    let dump = mir("fn main() { for i in 0..=3 { io.println(i) } }");
    assert!(dump.contains("Le(copy"), "inclusive uses Le:\n{}", dump);
}

#[test]
fn loop_with_break_value_yields_the_value() {
    let dump = mir("fn main() { let x = loop { break 7 }; io.println(x) }");
    assert!(!dump.contains("skipped"), "{}", dump);
    // `break 7` assigns the loop's result slot with const 7 (no `break` keyword
    // survives — it is a Goto to the join).
    assert!(dump.contains("const 7_i32"), "break value materialized:\n{}", dump);
}

#[test]
fn while_lowers_to_header_body_join() {
    let dump = mir("fn main() { let mut i = 0; while i < 3 { i = i + 1 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(dump.contains("Lt(copy"), "loop condition:\n{}", dump);
    assert!(dump.contains("Add(copy") && dump.contains("const 1"), "body increment:\n{}", dump);
}

#[test]
fn module_method_call_becomes_a_call_to_the_module_function() {
    let dump = mir("fn main() { io.println(\"hi\") }");
    assert!(dump.contains("call io.println(const \"hi\")"), "{}", dump);
}

#[test]
fn struct_literal_lowers_to_an_aggregate_in_declaration_order() {
    let src = "struct P { x: i32, y: i32 }\n\
               fn mk() -> P { P { y: 2, x: 1 } }";
    let dump = mir(src);
    // Fields emitted in declaration order (x then y) regardless of literal order.
    assert!(dump.contains("P(const 1_i32, const 2_i32)"), "{}", dump);
}

#[test]
fn match_function_is_skipped_with_a_reason() {
    let dump = mir("fn classify(n: i64) -> i64 { match n { 0 => 0, _ => 1 } }");
    assert!(
        dump.contains("skipped — `match` not lowered until Phase 3.3"),
        "{}",
        dump
    );
}

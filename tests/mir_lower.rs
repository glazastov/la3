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
    assert!(
        !dump.contains("skipped"),
        "fib/main must fully lower:\n{}",
        dump
    );
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
    assert!(
        dump.contains("if copy"),
        "branch on the condition:\n{}",
        dump
    );
    // Both arms assign the same result temp, which becomes _0.
    assert!(dump.contains("const 1_i32"), "{}", dump);
    assert!(dump.contains("const 2_i32"), "{}", dump);
    assert!(
        dump.contains("goto -> "),
        "arms jump to the join:\n{}",
        dump
    );
}

#[test]
fn for_over_exclusive_range_is_a_counter_loop() {
    let dump = mir("fn main() { for i in 0..3 { io.println(i) } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    // header compares with `<`, increment adds 1.
    assert!(dump.contains("Lt(copy"), "exclusive uses Lt:\n{}", dump);
    assert!(
        dump.contains("Add(copy") && dump.contains("const 1"),
        "increment:\n{}",
        dump
    );
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
    assert!(
        dump.contains("const 7_i32"),
        "break value materialized:\n{}",
        dump
    );
}

#[test]
fn while_lowers_to_header_body_join() {
    let dump = mir("fn main() { let mut i = 0; while i < 3 { i = i + 1 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(dump.contains("Lt(copy"), "loop condition:\n{}", dump);
    assert!(
        dump.contains("Add(copy") && dump.contains("const 1"),
        "body increment:\n{}",
        dump
    );
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

// -- match → decision trees (Phase 3.3) -----------------------------------

#[test]
fn match_on_int_literals_lowers_to_switches() {
    // Literal arms become `switch` over the scrutinee; the wildcard is the
    // exhaustive default, and the past-the-last-arm fall-through is unreachable.
    let dump = mir("fn classify(n: i64) -> i64 { match n { 0 => 10, 1 => 11, _ => 99 } }");
    assert!(!dump.contains("skipped"), "match must lower:\n{}", dump);
    assert!(!dump.contains("invalid MIR"), "must validate:\n{}", dump);
    assert!(
        dump.contains("switch copy"),
        "literal arms switch:\n{}",
        dump
    );
    assert!(
        dump.contains("switch copy _2 -> [0:"),
        "first literal switch:\n{}",
        dump
    );
    assert!(dump.contains("const 10"), "{}", dump);
    assert!(dump.contains("const 99"), "wildcard arm:\n{}", dump);
    assert!(
        dump.contains("unreachable"),
        "exhaustive fall-through:\n{}",
        dump
    );
}

#[test]
fn match_on_a_tuple_nests_switches_and_binds_fields() {
    // FizzBuzz's `classify`: a tuple of two ints with wildcards.
    let src = "fn classify(n: i64) -> str {\n\
               match (n % 3, n % 5) {\n\
                 (0, 0) => \"FizzBuzz\",\n\
                 (0, _) => \"Fizz\",\n\
                 (_, 0) => \"Buzz\",\n\
                 _      => \"n\",\n\
               }\n\
               }";
    let dump = mir(src);
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    // Tuple elements are projected as fields and switched on independently.
    assert!(
        dump.contains("switch copy _5.0"),
        "first element:\n{}",
        dump
    );
    assert!(
        dump.contains("switch copy _5.1"),
        "second element:\n{}",
        dump
    );
    assert!(dump.contains("const \"FizzBuzz\""), "{}", dump);
}

#[test]
fn match_binds_a_simple_name_arm() {
    // A bare-binding arm copies the scrutinee into a user local.
    let dump = mir("fn id(n: i64) -> i64 { match n { 0 => 0, other => other } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(
        dump.contains("[let]"),
        "binding arm materializes a local:\n{}",
        dump
    );
}

#[test]
fn match_on_an_enum_reads_the_discriminant_and_downcasts() {
    // shapes' `area`: tuple- and struct-variants of a user enum.
    let src = "enum Shape { Circle(f64), Rect { width: f64, height: f64 }, Tri(f64, f64, f64) }\n\
               fn area(s: Shape) -> f64 {\n\
                 match s {\n\
                   Shape.Circle(r) => r,\n\
                   Shape.Rect { width, height } => width,\n\
                   Shape.Tri(a, b, c) => a,\n\
                 }\n\
               }";
    let dump = mir(src);
    assert!(
        !dump.contains("skipped"),
        "enum match must lower:\n{}",
        dump
    );
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    assert!(
        dump.contains("discriminant(_2)"),
        "reads the tag:\n{}",
        dump
    );
    // Circle=0, Rect=1, Tri=2 in declaration order, with payload downcasts.
    assert!(
        dump.contains("switch copy _4 -> [0:"),
        "Circle is variant 0:\n{}",
        dump
    );
    assert!(
        dump.contains("(_2 as variant#0).0"),
        "Circle payload:\n{}",
        dump
    );
    assert!(
        dump.contains("(_2 as variant#1)"),
        "Rect downcast:\n{}",
        dump
    );
    assert!(
        dump.contains("(_2 as variant#2).2"),
        "Tri third field:\n{}",
        dump
    );
}

#[test]
fn match_on_a_result_uses_builtin_variant_order() {
    // The built-in `Result` (Ok=0, Err=1) with payload bindings.
    let src = "fn check(r: Result<i64>) -> i64 {\n\
               match r { Ok(n) => n, Err(e) => 0 }\n\
               }";
    let dump = mir(src);
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    assert!(dump.contains("discriminant("), "{}", dump);
    // Ok is variant 0: its switch target reads the Ok payload via downcast#0.
    assert!(dump.contains("-> [0:"), "Ok is variant 0:\n{}", dump);
    assert!(
        dump.contains("(_2 as variant#0).0"),
        "Ok payload:\n{}",
        dump
    );
}

#[test]
fn match_guard_falls_through_to_the_next_arm() {
    // A failed guard routes to the next arm, not the body.
    let dump = mir("fn f(n: i64) -> i64 { match n { x if x > 0 => 1, _ => 2 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    // The guard is a Gt comparison feeding an `if`.
    assert!(dump.contains("Gt(copy"), "guard comparison:\n{}", dump);
    assert!(dump.contains("if copy"), "guard branch:\n{}", dump);
}

#[test]
fn match_or_pattern_routes_each_alternative_to_the_body() {
    let dump = mir("fn f(n: i64) -> i64 { match n { 1 | 2 | 3 => 0, _ => 9 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    // Three alternatives ⇒ three single-target switches before the catch-all.
    let switches = dump.matches("switch copy").count();
    assert!(switches >= 3, "one switch per alternative:\n{}", dump);
}

#[test]
fn match_range_pattern_lowers_to_two_comparisons() {
    let dump = mir("fn f(n: i64) -> i64 { match n { 0..=9 => 1, _ => 2 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    assert!(dump.contains("Ge(copy"), "lower bound:\n{}", dump);
    assert!(dump.contains("Le(copy"), "inclusive upper bound:\n{}", dump);
}

#[test]
fn match_at_binding_tests_then_binds() {
    let dump = mir("fn f(n: i64) -> i64 { match n { x @ 1..=12 => x, _ => 0 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    // The range is tested (Ge/Le) and the whole value bound into a user local.
    assert!(
        dump.contains("Ge(copy") && dump.contains("Le(copy"),
        "{}",
        dump
    );
    assert!(dump.contains("[let]"), "@ binds a local:\n{}", dump);
}

#[test]
fn match_on_a_string_compares_for_equality() {
    let dump = mir("fn f(s: str) -> i64 { match s { \"a\" => 1, _ => 0 } }");
    assert!(!dump.contains("skipped"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
    assert!(
        dump.contains("Eq(copy") && dump.contains("const \"a\""),
        "{}",
        dump
    );
}

#[test]
fn match_with_an_unsupported_pattern_still_bails_honestly() {
    // List patterns need the runtime; the function is skipped, not mis-lowered.
    let dump = mir("fn f(xs: List<i64>) -> i64 { match xs { [a, b] => a, _ => 0 } }");
    assert!(dump.contains("skipped — list pattern"), "{}", dump);
    assert!(!dump.contains("invalid MIR"), "{}", dump);
}

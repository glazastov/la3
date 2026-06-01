//! Phase 1.4 — exact `as` cast and arithmetic semantics (reference Sections 3, 4).
//!
//! Two halves, both driven through the real binary:
//! * `check` cases pin down the *static* rule that `as` converts only between
//!   numeric types (and integer↔`char`); illegal casts must be rejected.
//! * `run` cases pin down the *runtime* exactness the spec states: integer `/`
//!   truncates toward zero, `%` takes the sign of its left operand, `idiv`
//!   (floor division) rounds toward negative infinity for every sign
//!   combination, `**` always yields `f64`, and `as` truncates / changes sign.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tmp(src: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_casts_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    file
}

/// Run `la3 check` on `src`, returning (combined output, success).
fn check(src: &str) -> (String, bool) {
    let file = tmp(src);
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
    assert!(success, "expected clean check, got errors:\n{}", text);
}

fn rejects(src: &str, needle: &str) {
    let (text, success) = check(src);
    assert!(
        !success,
        "expected a type error, but check passed:\n{}",
        text
    );
    assert!(
        text.contains(needle),
        "expected error to mention {:?}, got:\n{}",
        needle,
        text
    );
}

/// Run a program whose `main` prints a single value; return its trimmed stdout.
fn run_expr(body: &str) -> String {
    let src = format!("fn main() {{ io.println(str({})) }}", body);
    let file = tmp(&src);
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["run", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "program failed: `{}`\n{}",
        body,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// Static cast legality (Section 3)
// ---------------------------------------------------------------------------

#[test]
fn cast_numeric_to_numeric_is_allowed() {
    ok("fn main() { let n: i64 = 300; io.println(n as u8); io.println(n as f64) }");
}

#[test]
fn cast_between_integer_and_char_is_allowed() {
    ok("fn main() { io.println(65 as char); io.println('A' as i32) }");
}

#[test]
fn cast_str_to_number_is_rejected() {
    rejects(
        "fn main() { let s = \"hello\"; io.println(s as i32) }",
        "cannot cast",
    );
}

#[test]
fn cast_bool_to_number_is_rejected() {
    rejects("fn main() { io.println(true as f64) }", "cannot cast");
}

#[test]
fn cast_stays_lenient_on_generics() {
    // A generic parameter is not fully known, so a cast on it must not be a
    // false positive (the checker is sound-but-lenient).
    ok("fn id<T>(x: T) -> T { let _ = x as i64; x }\nfn main() { io.println(id(1)) }");
}

// ---------------------------------------------------------------------------
// Runtime exactness (Section 4)
// ---------------------------------------------------------------------------

#[test]
fn integer_division_truncates_toward_zero() {
    assert_eq!(run_expr("-7 / 2"), "-3");
    assert_eq!(run_expr("7 / 2"), "3");
}

#[test]
fn remainder_takes_sign_of_left_operand() {
    assert_eq!(run_expr("-7 % 2"), "-1");
    assert_eq!(run_expr("7 % -2"), "1");
}

#[test]
fn floor_division_rounds_toward_negative_infinity() {
    assert_eq!(run_expr("idiv(-7, 2)"), "-4");
    assert_eq!(run_expr("idiv(7, -2)"), "-4");
    assert_eq!(run_expr("idiv(7, 2)"), "3");
    assert_eq!(run_expr("idiv(-7, -2)"), "3");
}

#[test]
fn exponentiation_always_yields_f64() {
    // f64 renders with a trailing `.0` for whole values, proving the type.
    assert_eq!(run_expr("2 ** 10"), "1024.0");
}

#[test]
fn cast_truncates_and_changes_sign() {
    assert_eq!(run_expr("300 as u8"), "44"); // 300 mod 256
    assert_eq!(run_expr("-3.9 as i32"), "-3"); // float → int truncates toward zero
    assert_eq!(run_expr("65 as char"), "A");
    assert_eq!(run_expr("'A' as i32"), "65");
}

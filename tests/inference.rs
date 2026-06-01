//! Phase 1.5 — sound inference (reference Section 2, Type Inference Rules).
//!
//! * Rule 2: an unsuffixed integer literal defaults to `i32`, a float to `f64`.
//!   Verified through `la3 types`: the finished table must never leave a literal
//!   flexible (`{integer}`/`{float}`), and a literal in an annotated context must
//!   adopt that width.
//! * Rule 4: no implicit numeric widening or narrowing — mixing widths in
//!   arithmetic or assignment is a real type error (verified through `la3 check`).

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn write_tmp(src: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_infer_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    file
}

fn run(cmd: &str, src: &str) -> (String, bool) {
    let file = write_tmp(src);
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args([cmd, file.to_str().unwrap()])
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

/// The set of types `la3 types` records, one per annotated node.
fn types_of(src: &str) -> String {
    let (text, ok) = run("types", src);
    assert!(ok, "expected clean `types`, got:\n{}", text);
    text
}

fn rejects(src: &str, needle: &str) {
    let (text, ok) = run("check", src);
    assert!(!ok, "expected a type error, but check passed:\n{}", text);
    assert!(
        text.contains(needle),
        "expected error to mention {:?}, got:\n{}",
        needle,
        text
    );
}

// ---------------------------------------------------------------------------
// Rule 2 — literal defaults
// ---------------------------------------------------------------------------

#[test]
fn unconstrained_literals_default_to_i32_and_f64() {
    let dump = types_of("fn main() { let a = 42; let b = 3.14; io.println(a); io.println(b) }");
    // No literal may remain flexible in the finished table.
    assert!(
        !dump.contains("{integer}") && !dump.contains("{float}"),
        "literals were left flexible:\n{}",
        dump
    );
    assert!(dump.contains("i32"), "expected an i32 default:\n{}", dump);
    assert!(dump.contains("f64"), "expected an f64 default:\n{}", dump);
}

#[test]
fn no_example_leaves_a_flexible_literal() {
    // Every bundled example must type to a fully concrete table.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("la3") {
            continue;
        }
        let out = Command::new(env!("CARGO_BIN_EXE_la3"))
            .args(["types", path.to_str().unwrap()])
            .output()
            .expect("failed to launch la3");
        let dump = String::from_utf8_lossy(&out.stdout);
        assert!(
            !dump.contains("{integer}") && !dump.contains("{float}"),
            "{} left a flexible literal:\n{}",
            path.display(),
            dump
        );
    }
}

#[test]
fn annotated_literal_adopts_its_width() {
    // The literal node itself must be recorded at the annotated width, not the
    // i32 default (contextual pinning).
    let dump = types_of("fn main() { let x: u8 = 42; io.println(x) }");
    assert!(dump.contains("u8"), "literal did not adopt u8:\n{}", dump);
    assert!(
        !dump.contains("i32"),
        "annotated literal should not default to i32:\n{}",
        dump
    );
}

#[test]
fn array_element_literals_adopt_the_element_width() {
    let dump = types_of("fn main() { let xs: [u8; 3] = [1, 2, 3]; io.println(xs) }");
    // Three element nodes, each pinned to u8.
    assert!(
        dump.matches("u8").count() >= 3,
        "array element literals were not pinned to u8:\n{}",
        dump
    );
}

#[test]
fn literal_argument_adopts_the_parameter_width() {
    let dump = types_of("fn take(b: u16) { io.println(b) }\nfn main() { take(7) }");
    assert!(
        dump.contains("u16"),
        "argument literal not pinned:\n{}",
        dump
    );
}

// ---------------------------------------------------------------------------
// Rule 4 — no implicit widening / narrowing
// ---------------------------------------------------------------------------

#[test]
fn mixed_width_arithmetic_is_rejected() {
    rejects(
        "fn main() { let x: i32 = 1; let y: i64 = 2; let z = x + y; io.println(z) }",
        "must share a type",
    );
}

#[test]
fn widening_assignment_is_rejected() {
    rejects(
        "fn main() { let x: i32 = 1; let y: i64 = x; io.println(y) }",
        "type mismatch",
    );
}

#[test]
fn narrowing_assignment_is_rejected() {
    rejects(
        "fn main() { let x: i64 = 1; let y: i32 = x; io.println(y) }",
        "type mismatch",
    );
}

#[test]
fn explicit_cast_bridges_widths() {
    // The same mix is fine with an `as` cast (rule 4's escape hatch).
    let (text, ok) = run(
        "check",
        "fn main() { let x: i32 = 1; let y: i64 = 2; let z = x as i64 + y; io.println(z) }",
    );
    assert!(ok, "expected clean check with a cast, got:\n{}", text);
}

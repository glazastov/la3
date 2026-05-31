//! Smoke tests: every bundled example must parse, check, and run without error,
//! plus a few focused language-behavior assertions driven through the binary.

use std::process::Command;

fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(args)
        .output()
        .expect("failed to launch la3");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn all_examples_run() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("la3") {
            continue;
        }
        let p = path.to_str().unwrap();
        let (_out, err, ok) = run(&["run", p]);
        assert!(ok, "example {} failed to run:\n{}", p, err);
    }
}

/// `la3 types` must type every bundled example without error and emit at least
/// one `line:col  <type>` annotation per file (Phase 1.1: the expression type
/// table). This exercises NodeId numbering and the type table end to end.
#[test]
fn types_command_annotates_all_examples() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("la3") {
            continue;
        }
        let p = path.to_str().unwrap();
        let (out, err, ok) = run(&["types", p]);
        assert!(ok, "types failed for {}:\n{}", p, err);
        assert!(
            out.lines().any(|l| l.trim().contains(char::is_alphabetic)),
            "types produced no annotations for {}",
            p
        );
    }
}

#[test]
fn fib_output_is_correct() {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/fib.la3");
    let (out, _err, ok) = run(&["run", p]);
    assert!(ok);
    assert!(out.contains("fib(9) = 34"), "got:\n{}", out);
}

#[test]
fn check_reports_undefined_name() {
    let dir = std::env::temp_dir();
    let file = dir.join("la3_undef_test.la3");
    std::fs::write(&file, "fn main() { io.println(missing_name) }").unwrap();
    let (_out, err, ok) = run(&["check", file.to_str().unwrap()]);
    assert!(!ok, "check should fail on an undefined name");
    assert!(
        err.contains("undefined name 'missing_name'"),
        "got:\n{}",
        err
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn floor_division_builtin() {
    let dir = std::env::temp_dir();
    let file = dir.join("la3_idiv_test.la3");
    std::fs::write(&file, "fn main() { io.println(idiv(-7, 2)) }").unwrap();
    let (out, _err, ok) = run(&["run", file.to_str().unwrap()]);
    assert!(ok);
    assert_eq!(out.trim(), "-4");
    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// Section 11 — Pointer arithmetic runtime behaviour
// ---------------------------------------------------------------------------

/// `arr[i]` and `*(ptr + i)` must name the same element for every valid index.
#[test]
fn ptr_arithmetic_equivalence_with_array_index() {
    let src = r#"
fn main() {
    let arr: [i32; 5] = [10, 20, 30, 40, 50]
    let p: *i32 = &raw arr[0]
    let mut ok = true
    let mut i = 0
    while i < 5 {
        unsafe {
            if arr[i] != *(p + i) { ok = false }
        }
        i += 1
    }
    io.println(ok)
}
"#;
    let dir = std::env::temp_dir();
    let file = dir.join("la3_ptr_equiv_test.la3");
    std::fs::write(&file, src).unwrap();
    let (out, err, ok) = run(&["run", file.to_str().unwrap()]);
    assert!(ok, "run failed:\n{}", err);
    assert_eq!(out.trim(), "true", "arr[i] != *(p+i) somewhere:\n{}", out);
    let _ = std::fs::remove_file(&file);
}

/// `*(p + 2)` on a `*i32` array pointer returns the third element.
#[test]
fn ptr_plus_literal_reads_correct_element() {
    let src = r#"
fn main() {
    let arr: [i32; 5] = [10, 20, 30, 40, 50]
    let p: *i32 = &raw arr[0]
    unsafe {
        io.println(*(p + 0))
        io.println(*(p + 2))
        io.println(*(p + 4))
    }
}
"#;
    let dir = std::env::temp_dir();
    let file = dir.join("la3_ptr_lit_test.la3");
    std::fs::write(&file, src).unwrap();
    let (out, err, ok) = run(&["run", file.to_str().unwrap()]);
    assert!(ok, "run failed:\n{}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, ["10", "30", "50"], "got: {:?}", lines);
    let _ = std::fs::remove_file(&file);
}

/// `*(p - n)` steps backwards correctly.
#[test]
fn ptr_minus_integer_reads_correct_element() {
    let src = r#"
fn main() {
    let arr: [i32; 5] = [10, 20, 30, 40, 50]
    let p: *i32 = &raw arr[4]
    unsafe {
        io.println(*(p - 0))
        io.println(*(p - 2))
        io.println(*(p - 4))
    }
}
"#;
    let dir = std::env::temp_dir();
    let file = dir.join("la3_ptr_sub_test.la3");
    std::fs::write(&file, src).unwrap();
    let (out, err, ok) = run(&["run", file.to_str().unwrap()]);
    assert!(ok, "run failed:\n{}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, ["50", "30", "10"], "got: {:?}", lines);
    let _ = std::fs::remove_file(&file);
}

/// `*mut u8` pointer with alloc: write and read back through offset arithmetic.
#[test]
fn mut_u8_ptr_write_read_through_offsets() {
    let src = r#"
fn main() {
    let buf: *mut u8 = alloc(4)
    unsafe {
        *buf       = 10
        *(buf + 1) = 20
        *(buf + 2) = 30
        *(buf + 3) = 40
        io.println(*buf)
        io.println(*(buf + 1))
        io.println(*(buf + 2))
        io.println(*(buf + 3))
    }
    dealloc(buf, 4)
}
"#;
    let dir = std::env::temp_dir();
    let file = dir.join("la3_ptr_u8_test.la3");
    std::fs::write(&file, src).unwrap();
    let (out, err, ok) = run(&["run", file.to_str().unwrap()]);
    assert!(ok, "run failed:\n{}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, ["10", "20", "30", "40"], "got: {:?}", lines);
    let _ = std::fs::remove_file(&file);
}

/// The bundled memory.la3 example must check and run cleanly.
#[test]
fn memory_example_runs_cleanly() {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/memory.la3");
    let (_out, err, ok) = run(&["run", p]);
    assert!(ok, "memory.la3 failed:\n{}", err);
}


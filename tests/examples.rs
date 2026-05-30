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
    assert!(err.contains("undefined name 'missing_name'"), "got:\n{}", err);
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

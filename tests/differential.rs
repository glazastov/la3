//! Differential test harness: the compiled binary must behave exactly like the
//! interpreter (our correctness oracle). For every bundled example we capture
//! the interpreter's stdout + exit code, then — once the LLVM backend emits
//! binaries — run the compiled program and assert the two match.
//!
//! Until codegen lands (Phase 4), `la3 build` is a stub that produces no binary
//! and exits with [`CODEGEN_PENDING`]. The harness detects that and **skips** the
//! compiled comparison, so this file is wired now and starts enforcing parity
//! automatically the moment `build` begins emitting executables.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit code `la3 build` returns while the native backend is not implemented.
/// Keep in sync with `src/main.rs`.
const CODEGEN_PENDING: i32 = 3;

struct Run {
    stdout: String,
    code: Option<i32>,
}

fn la3(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(args)
        .output()
        .expect("failed to launch la3")
}

/// Run a program through the interpreter and capture its observable behaviour.
fn interp_run(src: &Path) -> Run {
    let out = la3(&["run", src.to_str().unwrap()]);
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        code: out.status.code(),
    }
}

/// Try to compile `src` to a native binary at `bin`. Returns:
/// - `Ok(true)`  — a binary was produced and is ready to run;
/// - `Ok(false)` — codegen is not implemented yet (stub), skip the comparison;
/// - `Err(msg)`  — build failed for a real reason (a test failure).
fn try_build(src: &Path, bin: &Path) -> Result<bool, String> {
    let out = la3(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
    if out.status.code() == Some(CODEGEN_PENDING) {
        return Ok(false);
    }
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(bin.exists())
}

/// Run a compiled binary and capture its observable behaviour.
fn binary_run(bin: &Path) -> Run {
    let out = Command::new(bin)
        .output()
        .expect("failed to launch compiled binary");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        code: out.status.code(),
    }
}

fn examples() -> Vec<PathBuf> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("la3"))
        .collect();
    v.sort();
    v
}

/// The core invariant: compiled output == interpreter output. Skips, with a
/// printed notice, any example whose backend is not ready yet.
#[test]
fn compiled_matches_interpreter() {
    let tmp = std::env::temp_dir();
    let mut compared = 0;
    let mut skipped = 0;

    for ex in examples() {
        let name = ex.file_stem().unwrap().to_str().unwrap();
        let oracle = interp_run(&ex);
        let bin = tmp.join(format!("la3_diff_{name}"));
        let _ = std::fs::remove_file(&bin);

        match try_build(&ex, &bin) {
            Ok(false) => {
                skipped += 1;
                continue;
            }
            Err(msg) => panic!("build failed for {}:\n{msg}", ex.display()),
            Ok(true) => {}
        }

        let compiled = binary_run(&bin);
        assert_eq!(
            compiled.stdout,
            oracle.stdout,
            "stdout mismatch for {}: interpreter vs compiled",
            ex.display()
        );
        assert_eq!(
            compiled.code,
            oracle.code,
            "exit-code mismatch for {}: interpreter vs compiled",
            ex.display()
        );
        compared += 1;
        let _ = std::fs::remove_file(&bin);
    }

    eprintln!("differential: {compared} compared, {skipped} skipped (codegen pending)");
    // While the backend is unimplemented, every example is skipped; that is the
    // expected Phase 0 state, not a failure.
}

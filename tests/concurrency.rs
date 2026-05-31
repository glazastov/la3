//! Concurrency tests (reference Section 12).
//!
//! The interpreter runs spawned tasks cooperatively to completion, so these
//! tests assert the *observable* behavior of channels, `spawn`/`join`, and the
//! `all` / `race` await primitives. Each case drives the real binary.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn run_src(src: &str) -> (String, String, bool) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = std::env::temp_dir().join(format!("la3_conc_{}_{}.la3", std::process::id(), n));
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_la3"))
        .args(["run", file.to_str().unwrap()])
        .output()
        .expect("failed to launch la3");
    let _ = std::fs::remove_file(&file);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn channel_producer_consumer() {
    let (out, err, ok) = run_src(
        "fn main() {\n\
         let ch = channel<i64>(capacity: 4)\n\
         spawn { for i in 1..=3 { ch.send(i * 10) } ch.close() }\n\
         for v in ch { io.println(v) }\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert_eq!(out, "10\n20\n30\n");
}

#[test]
fn spawn_join_returns_task_value() {
    let (out, err, ok) = run_src(
        "fn main() {\n\
         let h = spawn { 6 * 7 }\n\
         io.println(h.join())\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert_eq!(out.trim(), "42");
}

#[test]
fn await_all_resolves_in_order() {
    let (out, err, ok) = run_src(
        "fn main() {\n\
         let r = await all([spawn { 1 }, spawn { 2 }, spawn { 3 }])\n\
         io.println(r)\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert_eq!(out.trim(), "[1, 2, 3]");
}

#[test]
fn await_race_takes_first() {
    let (out, err, ok) = run_src(
        "fn main() {\n\
         let w = await race(spawn { 100 }, spawn { 200 })\n\
         io.println(w)\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert_eq!(out.trim(), "100");
}

#[test]
fn fire_and_forget_task_runs_at_shutdown() {
    // A spawned task that is never joined still runs (its side effects happen).
    let (out, err, ok) = run_src(
        "fn main() {\n\
         spawn { io.println(\"bg\") }\n\
         io.println(\"main\")\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert!(out.contains("bg"), "background task did not run:\n{}", out);
    assert!(out.contains("main"));
}

#[test]
fn recv_returns_none_after_close() {
    // `recv` yields an Option: Some while data remains, then nil once closed.
    let (out, err, ok) = run_src(
        "fn main() {\n\
         let ch = channel<i64>()\n\
         spawn { ch.send(7) ch.close() }\n\
         io.println(ch.recv().unwrap())\n\
         io.println(ch.recv() == nil)\n\
         }",
    );
    assert!(ok, "stderr:\n{}", err);
    assert_eq!(out, "7\ntrue\n");
}

#[test]
fn empty_open_channel_deadlocks() {
    // Receiving from an empty channel that nothing will fill is a deadlock.
    let (_out, err, ok) = run_src(
        "fn main() {\n\
         let ch = channel<i64>()\n\
         for x in ch { io.println(x) }\n\
         }",
    );
    assert!(!ok, "expected a deadlock error");
    assert!(err.contains("deadlock"), "stderr:\n{}", err);
}

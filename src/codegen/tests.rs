//! Back-end tests (IR emit/link smoke + JIT execution batteries). Split out of `codegen.rs`.

use super::*;

/// `target/debug` (where `cargo` puts `libla3_runtime.a`), derived from the
/// running test binary at `target/debug/deps/<test>`.
fn target_debug_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent() // deps/
        .and_then(|p| p.parent()) // debug/
        .expect("target/debug")
        .to_path_buf()
}

#[test]
fn module_emits_valid_ir_with_main_and_runtime_call() {
    let ir = emit_ir().expect("emit IR");
    // The module verified (emit_ir checks `module.verify()`); confirm it has
    // the two symbols the scaffold is about.
    assert!(ir.contains("define i32 @main()"), "main is defined:\n{ir}");
    assert!(
        ir.contains("declare i32 @la3_runtime_version()"),
        "runtime symbol declared:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @la3_runtime_version()"),
        "main calls it"
    );
}

#[test]
fn object_links_against_runtime_and_runs() {
    // Ensure the runtime staticlib is present (built by `cargo test
    // --workspace`); build it on demand otherwise so `-p la3` also works.
    let dir = target_debug_dir();
    let lib = dir.join("libla3_runtime.a");
    if !lib.exists() {
        let ok = Command::new(env!("CARGO"))
            .args(["build", "-p", "la3_runtime"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok && lib.exists(), "could not build la3_runtime staticlib");
    }

    let tmp = std::env::temp_dir();
    let obj = tmp.join(format!("la3_codegen_smoke_{}.o", std::process::id()));
    let bin = tmp.join(format!("la3_codegen_smoke_{}", std::process::id()));

    emit_object(&obj).expect("emit object");
    link_executable(&obj, &bin, &dir).expect("link against runtime");

    let status = Command::new(&bin).status().expect("run linked binary");
    assert_eq!(
        status.code(),
        Some(RUNTIME_VERSION),
        "the linked binary returns the runtime version, proving the runtime \
             was linked and called"
    );

    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&bin);
}

// -- Phase 5.2: MIR → LLVM scalar/arithmetic translation, executed via JIT --
//
// The test inputs are deliberately within each type's range, so the
// width-exact compiled result equals the interpreter oracle's i64/f64 result
// (they only diverge at narrow-width overflow). Asserting the plainly-computed
// value is therefore asserting oracle parity for these inputs.

use inkwell::execution_engine::JitFunction;

/// Lower a complete La3 source to MIR + the layout oracle codegen needs.
fn lower_to_mir(src: &str) -> (crate::mir::MirProgram, LayoutOracle) {
    let prog = crate::parser::parse(src).expect("parse");
    let errs = crate::checker::check(&prog);
    assert!(errs.is_empty(), "front-end errors: {errs:?}");
    let res = crate::checker::resolve(&prog);
    let table = crate::typeck::check_types(&prog);
    let hir = crate::hir::lower(&prog, &table, &res);
    let mir = crate::mirgen::lower(&hir).program;
    (mir, crate::typeck::layout_oracle(&prog))
}

/// Build the LLVM module for `src` and return a JIT engine over it.
fn jit<'ctx>(ctx: &'ctx Context, src: &str) -> inkwell::execution_engine::ExecutionEngine<'ctx> {
    let (mir, oracle) = lower_to_mir(src);
    let (module, _skipped) = build_program_module(ctx, &mir, &oracle).expect("build module");
    Target::initialize_native(&InitializationConfig::default()).expect("init native");
    module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("jit engine")
}

#[test]
fn int_arithmetic_and_division_signs() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn f(a: i32, b: i32) -> i32 { a * b - 100 }\n\
             fn d(a: i32, b: i32) -> i32 { a / b }\n\
             fn r(a: i32, b: i32) -> i32 { a % b }\n\
             fn main() {}",
    );
    unsafe {
        let f: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> = ee.get_function("f").unwrap();
        assert_eq!(f.call(6, 7), -58);
        let d: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> = ee.get_function("d").unwrap();
        // `/` truncates toward zero (oracle: `a / b`).
        assert_eq!(d.call(-7, 2), -3);
        let r: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> = ee.get_function("r").unwrap();
        // `%` takes the sign of the left operand (oracle: `a % b`).
        assert_eq!(r.call(-7, 2), -1);
    }
}

#[test]
fn unsigned_division_uses_udiv() {
    // 4_000_000_000 > i32::MAX, so a signed `sdiv` would give a different
    // answer — this pins that unsigned types lower to `udiv`.
    let ctx = Context::create();
    let ee = jit(&ctx, "fn ud(a: u32, b: u32) -> u32 { a / b }\nfn main() {}");
    unsafe {
        let ud: JitFunction<unsafe extern "C" fn(u32, u32) -> u32> = ee.get_function("ud").unwrap();
        assert_eq!(ud.call(4_000_000_000, 7), 4_000_000_000 / 7);
    }
}

#[test]
fn float_arithmetic_and_pow() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn g(x: f64, y: f64) -> f64 { x / y + 1.0 }\n\
             fn p(x: f64, y: f64) -> f64 { x ** y }\n\
             fn main() {}",
    );
    unsafe {
        let g: JitFunction<unsafe extern "C" fn(f64, f64) -> f64> = ee.get_function("g").unwrap();
        assert_eq!(g.call(1.0, 4.0), 1.25);
        let p: JitFunction<unsafe extern "C" fn(f64, f64) -> f64> = ee.get_function("p").unwrap();
        // `**` always yields f64 (oracle: `a.powf(b)`).
        assert_eq!(p.call(2.0, 10.0), 1024.0);
    }
}

#[test]
fn casts_and_bitwise() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn c(x: f64) -> i32 { x as i32 }\n\
             fn sh(a: i32, b: i32) -> i32 { (a << b) | 1 }\n\
             fn main() {}",
    );
    unsafe {
        let c: JitFunction<unsafe extern "C" fn(f64) -> i32> = ee.get_function("c").unwrap();
        // float → int truncates toward zero (oracle).
        assert_eq!(c.call(3.9), 3);
        assert_eq!(c.call(-3.9), -3);
        let sh: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> = ee.get_function("sh").unwrap();
        assert_eq!(sh.call(1, 4), (1 << 4) | 1);
    }
}

#[test]
fn comparison_returns_bool() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn lt(a: i32, b: i32) -> bool { a < b }\nfn main() {}",
    );
    unsafe {
        let lt: JitFunction<unsafe extern "C" fn(i32, i32) -> bool> =
            ee.get_function("lt").unwrap();
        assert!(lt.call(2, 3));
        assert!(!lt.call(3, 3));
    }
}

#[test]
fn direct_call_between_functions() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn add(a: i32, b: i32) -> i32 { a + b }\n\
             fn use_add(x: i32) -> i32 { add(x, 1) }\n\
             fn main() {}",
    );
    unsafe {
        let use_add: JitFunction<unsafe extern "C" fn(i32) -> i32> =
            ee.get_function("use_add").unwrap();
        assert_eq!(use_add.call(41), 42);
    }
}

#[test]
fn out_of_scope_function_is_skipped_not_miscompiled() {
    // A `&mut` reference parameter is still beyond scope (Phase 6.3), so the
    // function must be reported skipped rather than mis-translated. (`str` is
    // now supported as of 6.1, so it is no longer the example here.)
    let ctx = Context::create();
    let (mir, oracle) = lower_to_mir("fn bump(x: &mut i32) { *x = *x + 1 }\nfn main() {}");
    let (_module, skipped) = build_program_module(&ctx, &mir, &oracle).expect("build module");
    assert!(
        skipped.iter().any(|(sym, _)| sym == "bump"),
        "ref-taking fn `bump` reported skipped: {skipped:?}"
    );
}

#[test]
fn str_returning_function_is_supported() {
    // Phase 6.1: a `str`-returning function is now compilable (not skipped).
    let ctx = Context::create();
    let (mir, oracle) = lower_to_mir("fn s() -> str { \"hi\" }\nfn main() {}");
    let (_module, skipped) = build_program_module(&ctx, &mir, &oracle).expect("build module");
    assert!(
        !skipped.iter().any(|(sym, _)| sym == "s"),
        "str-returning fn `s` should be supported now: {skipped:?}"
    );
}

// -- Phase 5.3: control flow from the MIR CFG (if/switch, loops, break-value).

#[test]
fn recursion_and_if_expression() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\n\
             fn main() {}",
    );
    unsafe {
        let fib: JitFunction<unsafe extern "C" fn(i64) -> i64> = ee.get_function("fib").unwrap();
        assert_eq!(fib.call(0), 0);
        assert_eq!(fib.call(1), 1);
        assert_eq!(fib.call(10), 55);
    }
}

#[test]
fn for_loop_and_while_loop() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn sum(n: i32) -> i32 { let mut acc = 0; for i in 1..=n { acc = acc + i }; acc }\n\
             fn count_down(n: i32) -> i32 { let mut x = n; let mut steps = 0; \
                 while x > 0 { x = x - 1; steps = steps + 1 }; steps }\n\
             fn main() {}",
    );
    unsafe {
        let sum: JitFunction<unsafe extern "C" fn(i32) -> i32> = ee.get_function("sum").unwrap();
        assert_eq!(sum.call(100), 5050); // 1..=100
        let cd: JitFunction<unsafe extern "C" fn(i32) -> i32> =
            ee.get_function("count_down").unwrap();
        assert_eq!(cd.call(7), 7);
    }
}

#[test]
fn loop_break_with_value() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn first(n: i32) -> i32 { let mut i = 0; loop { if i >= n { break i } i = i + 1 } }\n\
             fn main() {}",
    );
    unsafe {
        let first: JitFunction<unsafe extern "C" fn(i32) -> i32> =
            ee.get_function("first").unwrap();
        assert_eq!(first.call(5), 5);
        assert_eq!(first.call(0), 0);
    }
}

#[test]
fn integer_match_lowers_to_branches() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn classify(n: i32) -> i32 { match n { 0 => 100, 1 => 200, _ => 999 } }\n\
             fn main() {}",
    );
    unsafe {
        let c: JitFunction<unsafe extern "C" fn(i32) -> i32> = ee.get_function("classify").unwrap();
        assert_eq!(c.call(0), 100);
        assert_eq!(c.call(1), 200);
        assert_eq!(c.call(7), 999);
    }
}

// -- Phase 5.4: structs/tuples by value, enums as tagged unions, match trees.

#[test]
fn tuple_build_and_field_access() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "fn tup(a: i32, b: i32) -> i32 { let p = (a, b); p.0 * 10 + p.1 }\nfn main() {}",
    );
    unsafe {
        let tup: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
            ee.get_function("tup").unwrap();
        assert_eq!(tup.call(3, 4), 34);
    }
}

#[test]
fn struct_build_and_field_access() {
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "struct Pt { x: i32, y: i32 }\n\
             fn st(a: i32, b: i32) -> i32 { let p = Pt { x: a, y: b }; p.x - p.y }\n\
             fn main() {}",
    );
    unsafe {
        let st: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> = ee.get_function("st").unwrap();
        assert_eq!(st.call(10, 3), 7);
    }
}

#[test]
fn enum_construct_pass_by_value_and_match() {
    // `area(s: Shape)` takes the enum **by value**; the wrappers build a
    // variant and pass it, exercising tagged-union construction, by-value
    // aggregate arguments, and a match (discriminant switch + payload
    // downcast). Tuple variants only — mirgen does not yet lower
    // struct-variant *construction* (it does lower struct-variant matches).
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "enum Shape { Circle(f64), Rect(f64, f64) }\n\
             fn area(s: Shape) -> f64 { match s { Shape.Circle(r) => r * r, Shape.Rect(w, h) => w * h } }\n\
             fn circle_area(r: f64) -> f64 { area(Shape.Circle(r)) }\n\
             fn rect_area(w: f64, h: f64) -> f64 { area(Shape.Rect(w, h)) }\n\
             fn main() {}",
    );
    unsafe {
        let circle: JitFunction<unsafe extern "C" fn(f64) -> f64> =
            ee.get_function("circle_area").unwrap();
        assert_eq!(circle.call(2.0), 4.0);
        let rect: JitFunction<unsafe extern "C" fn(f64, f64) -> f64> =
            ee.get_function("rect_area").unwrap();
        assert_eq!(rect.call(3.0, 4.0), 12.0);
    }
}

#[test]
fn enum_built_and_matched_within_one_function() {
    // No aggregate ABI: build a variant into a local (through an `if` that
    // yields an enum, copied by bytes), then match it — all in one function.
    let ctx = Context::create();
    let ee = jit(
        &ctx,
        "enum Shape { Circle(f64), Rect(f64, f64) }\n\
             fn classify(kind: i32, a: f64, b: f64) -> f64 {\n\
                 let s = if kind == 0 { Shape.Circle(a) } else { Shape.Rect(a, b) }\n\
                 match s { Shape.Circle(r) => r * r, Shape.Rect(w, h) => w * h }\n\
             }\n\
             fn main() {}",
    );
    unsafe {
        let c: JitFunction<unsafe extern "C" fn(i32, f64, f64) -> f64> =
            ee.get_function("classify").unwrap();
        assert_eq!(c.call(0, 5.0, 0.0), 25.0); // Circle(5) → 25
        assert_eq!(c.call(1, 3.0, 4.0), 12.0); // Rect(3,4) → 12
    }
}

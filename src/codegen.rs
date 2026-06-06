//! LLVM back-end (Phase 5) — MIR → LLVM IR → object → linked executable.
//!
//! This is intentionally a *thin* translation layer: all the hard lowerings
//! already happened in MIR (Phase 3), so codegen stays mechanical. Phase 5.1 is
//! the **scaffold**: it wires `inkwell`/LLVM 18 into the build and proves the
//! whole tail of the pipeline end-to-end — emit a valid LLVM module, write a
//! native object with the target machine, and link it against the `la3_runtime`
//! static library into a runnable executable. It does **not** translate the
//! user program yet (functions, control flow, etc. land in 5.2+), so the module
//! it emits is the minimal one that still exercises object emission *and*
//! runtime linkage: a `main` that calls the runtime's `la3_runtime_version()`
//! and returns it as the process exit code.
//!
//! LLVM is reached through `inkwell` with the `llvm18-1-prefer-dynamic` feature
//! (see Cargo.toml); the install lives at `/usr/lib/llvm-18` via
//! `LLVM_SYS_181_PREFIX` (.cargo/config.toml).

// Like `ast.rs`/`mir.rs`: the emit/link API is exercised by this module's tests
// now and wired into the `build` command across 5.2–5.5 as real MIR→IR
// translation lands.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use inkwell::OptimizationLevel;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};

/// The exit code the runtime's smoke symbol returns (`la3_runtime_version`), so
/// the linked binary's exit status proves the runtime was linked and called.
pub const RUNTIME_VERSION: i32 = 1;

/// Build the Phase 5.1 scaffold module in `ctx`: declare the external runtime
/// symbol `la3_runtime_version` and define `i32 @main()` returning its result.
fn build_main_module<'ctx>(ctx: &'ctx Context) -> Result<Module<'ctx>, String> {
    let module = ctx.create_module("la3_main");
    let i32t = ctx.i32_type();

    // declare i32 @la3_runtime_version()
    let ver_fn = module.add_function("la3_runtime_version", i32t.fn_type(&[], false), None);

    // define i32 @main() { ret (call @la3_runtime_version()) }
    let main_fn = module.add_function("main", i32t.fn_type(&[], false), None);
    let entry = ctx.append_basic_block(main_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry);
    let call = builder
        .build_call(ver_fn, &[], "v")
        .map_err(|e| format!("build_call failed: {e}"))?;
    let ret = call
        .try_as_basic_value()
        .basic()
        .ok_or("la3_runtime_version returned void")?;
    builder
        .build_return(Some(&ret))
        .map_err(|e| format!("build_return failed: {e}"))?;
    Ok(module)
}

/// Emit the scaffold module as textual LLVM IR (used for a linker-independent
/// check that the module is well-formed and contains the expected symbols).
pub fn emit_ir() -> Result<String, String> {
    let ctx = Context::create();
    let module = build_main_module(&ctx)?;
    module
        .verify()
        .map_err(|e| format!("LLVM module verification failed: {e}"))?;
    Ok(module.print_to_string().to_string())
}

/// Emit the scaffold module to a native object file at `out_obj`.
pub fn emit_object(out_obj: &Path) -> Result<(), String> {
    let ctx = Context::create();
    let module = build_main_module(&ctx)?;
    module
        .verify()
        .map_err(|e| format!("LLVM module verification failed: {e}"))?;

    Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| format!("LLVM native target init failed: {e}"))?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
    let tm = target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::None,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or("could not create the LLVM target machine")?;
    module.set_triple(&triple);
    module.set_data_layout(&tm.get_target_data().get_data_layout());
    tm.write_to_file(&module, FileType::Object, out_obj)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Link `obj` against the `la3_runtime` static library (found under
/// `runtime_dir`) into the executable `out_bin`, driving the system linker
/// through `cc`. The runtime is a Rust `staticlib`, so the final link also
/// needs the C-runtime system libraries it depends on.
pub fn link_executable(obj: &Path, out_bin: &Path, runtime_dir: &Path) -> Result<(), String> {
    let status = Command::new("cc")
        .arg(obj)
        .arg("-o")
        .arg(out_bin)
        .arg(format!("-L{}", runtime_dir.display()))
        .arg("-lla3_runtime")
        // System libraries the Rust staticlib pulls in.
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm")
        .status()
        .map_err(|e| format!("failed to invoke cc: {e}"))?;
    if !status.success() {
        return Err(format!("cc failed: {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        assert!(ir.contains("call i32 @la3_runtime_version()"), "main calls it");
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
}

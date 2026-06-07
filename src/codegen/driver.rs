//! LLVM back-end driver: the Phase 5.1 scaffold module, native-object emission,
//! linking, and the `build` pipeline (`compile_executable`). Split out of `codegen.rs`.

use super::*;

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
    write_module_object(&module, out_obj)
}

/// Write an already-built (and verified-able) LLVM `module` to a native object
/// file at `out_obj`, through the default-target `TargetMachine`. Shared by the
/// scaffold ([`emit_object`]) and the real program driver ([`compile_executable`]).
fn write_module_object(module: &Module, out_obj: &Path) -> Result<(), String> {
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
    tm.write_to_file(module, FileType::Object, out_obj)
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

/// Build the full **executable** module for `prog`: the translated functions
/// ([`build_program_module`]) plus a C `i32 @main()` entry point that calls the
/// La3 `main` and returns its **exit code** (an integer `main` → that value; a
/// unit `main` → 0). Returns the module, the skipped list, and `entry_ok` —
/// whether a runnable entry was produced (true iff La3 `main` was compilable).
/// When `entry_ok` is false the program uses features the back-end can't lower
/// yet (`str`/`io`/collections — Phase 6), so the driver reports it as pending.
pub fn build_executable_module<'ctx>(
    ctx: &'ctx Context,
    prog: &MirProgram,
    oracle: &LayoutOracle,
) -> Result<(Module<'ctx>, Vec<(String, String)>, bool), String> {
    let (module, skipped) = build_program_module(ctx, prog, oracle)?;
    // The La3 `main` is a free function, emitted under the LLVM name `main`. It
    // is present iff it was supported (skipped functions are never declared).
    let entry_ok = match module.get_function("main") {
        Some(la3_main) => {
            add_c_entry(ctx, &module, la3_main)?;
            module
                .verify()
                .map_err(|e| format!("LLVM module verification failed after adding entry: {e}"))?;
            true
        }
        None => false,
    };
    Ok((module, skipped, entry_ok))
}

/// Rename the La3 `main` to `la3_main` and synthesize a C `i32 @main()` that
/// calls it and returns the process exit code: an integer return is normalized
/// to `i32`; a unit (`void`) return yields `0`.
fn add_c_entry<'ctx>(
    ctx: &'ctx Context,
    module: &Module<'ctx>,
    la3_main: FunctionValue<'ctx>,
) -> Result<(), String> {
    la3_main.as_global_value().set_name("la3_main");
    let i32t = ctx.i32_type();
    let main_fn = module.add_function("main", i32t.fn_type(&[], false), None);
    let entry = ctx.append_basic_block(main_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry);
    let call = builder
        .build_call(la3_main, &[], "r")
        .map_err(|e| format!("entry build_call failed: {e}"))?;
    // Normalize the return to an i32 exit code (or 0 for a unit `main`).
    let code = match call.try_as_basic_value().basic() {
        Some(BasicValueEnum::IntValue(iv)) => builder
            .build_int_cast_sign_flag(iv, i32t, true, "code")
            .map_err(|e| format!("exit-code cast failed: {e}"))?,
        _ => i32t.const_zero(),
    };
    builder
        .build_return(Some(&code))
        .map_err(|e| format!("entry build_return failed: {e}"))?;
    Ok(())
}

/// End-to-end driver for the `build` command: lower `prog` to an executable
/// module, write an object, and link it against the `la3_runtime` static library
/// (found next to the running `la3` binary) into `out_bin`. Returns:
/// - `Ok(true)`  — a native binary was produced;
/// - `Ok(false)` — the program uses features the back-end can't lower yet
///   (so `main` did not compile); the caller reports this as codegen-pending;
/// - `Err(msg)`  — a real codegen/link failure.
pub fn compile_executable(
    prog: &MirProgram,
    oracle: &LayoutOracle,
    out_bin: &Path,
) -> Result<bool, String> {
    let ctx = Context::create();
    let (module, _skipped, entry_ok) = build_executable_module(&ctx, prog, oracle)?;
    if !entry_ok {
        return Ok(false);
    }
    let runtime_dir = runtime_dir()?;
    // Object next to the requested output, named after it (kept after the link).
    let obj = out_bin.with_extension("o");
    write_module_object(&module, &obj)?;
    link_executable(&obj, out_bin, &runtime_dir)?;
    let _ = std::fs::remove_file(&obj);
    Ok(true)
}

/// The directory holding the `la3_runtime` static library — the directory of the
/// running `la3` executable (`target/<profile>/`, where cargo also puts
/// `libla3_runtime.a`).
fn runtime_dir() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    exe.parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "could not locate the la3 executable's directory".into())
}

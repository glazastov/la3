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

// ===========================================================================
// MIR → LLVM IR translation (Phase 5.2: functions, params, return, scalars,
// arithmetic; Phase 5.3: control flow — the `if`/`switch` CFG branches, with
// loop-carried values flowing through the per-local `alloca`s, so no φ-nodes are
// needed). Aggregates, references, and the runtime/heap calls are later
// subparts; a MIR function that uses them is reported as *skipped* (mirroring
// how `mirgen` bails), never mis-translated.
// ===========================================================================

use std::collections::HashMap;

use inkwell::builder::Builder;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, PointerValue,
};
use inkwell::{FloatPredicate, IntPredicate};

use crate::ast::{BinOp, UnOp};
use crate::mir::{
    AggregateKind, BasicBlock, Const, MirFn, MirProgram, Operand, Place, Projection, Rvalue,
    Statement, Terminator,
};
use crate::ty::{FloatKind, IntKind, Ty};
use crate::typeck::LayoutOracle;

/// The final symbol of a MIR function: `Owner::method` for a method, the bare
/// name for a free function — matching the `Const::Fn` symbol a call carries.
fn fn_symbol(f: &MirFn) -> String {
    match &f.owner {
        Some(o) => format!("{}::{}", o, f.name),
        None => f.name.clone(),
    }
}

/// Whether an integer kind is signed (selects `sdiv`/`srem`/`ashr` and the
/// signed `icmp` predicates).
fn is_signed(k: IntKind) -> bool {
    matches!(
        k,
        IntKind::I8 | IntKind::I16 | IntKind::I32 | IntKind::I64 | IntKind::Isize
    )
}

/// Lower an entire MIR program to an LLVM module. Returns the module plus the
/// list of `(symbol, reason)` for functions the back-end cannot translate yet.
/// `oracle` answers the by-value layout questions (struct/tuple/enum geometry).
pub fn build_program_module<'ctx>(
    ctx: &'ctx Context,
    prog: &MirProgram,
    oracle: &LayoutOracle,
) -> Result<(Module<'ctx>, Vec<(String, String)>), String> {
    let module = ctx.create_module("la3");

    // Pass 1: which functions can we translate? A function is supported only if
    // its own shape is supported *and* every function it calls is too (else the
    // call would reference an undefined symbol). Compute that as a fixpoint.
    let mut skipped: HashMap<String, String> = HashMap::new();
    let known: std::collections::HashSet<String> = prog.fns.iter().map(fn_symbol).collect();
    for f in &prog.fns {
        if let Some(reason) = unsupported_reason(f, oracle) {
            skipped.insert(fn_symbol(f), reason);
        }
    }
    loop {
        let mut changed = false;
        for f in &prog.fns {
            let sym = fn_symbol(f);
            if skipped.contains_key(&sym) {
                continue;
            }
            for callee in call_targets(f, oracle) {
                let bad = !known.contains(&callee) || skipped.contains_key(&callee);
                if bad {
                    skipped.insert(sym.clone(), format!("calls unsupported fn `{callee}`"));
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Parameter types per function symbol (used to type call arguments).
    let sigs: HashMap<String, Vec<Ty>> = prog
        .fns
        .iter()
        .map(|f| {
            (
                fn_symbol(f),
                (1..=f.arg_count).map(|i| f.locals[i].ty.clone()).collect(),
            )
        })
        .collect();

    // Pass 2: declare the signatures of every supported function.
    let mut decls: HashMap<String, FunctionValue> = HashMap::new();
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if skipped.contains_key(&sym) {
            continue;
        }
        let fn_ty = fn_type(ctx, f, oracle);
        decls.insert(sym.clone(), module.add_function(&sym, fn_ty, None));
    }

    // Pass 3: translate the bodies.
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if let Some(&fval) = decls.get(&sym) {
            let mut g = FnGen {
                ctx,
                module: &module,
                oracle,
                builder: ctx.create_builder(),
                decls: &decls,
                sigs: &sigs,
                f,
                fval,
                local_types: infer_local_types(f, oracle),
                slots: Vec::new(),
                blocks: Vec::new(),
            };
            g.gen_fn()?;
        }
    }

    module
        .verify()
        .map_err(|e| format!("LLVM module verification failed for la3 module: {e}"))?;

    let mut skipped: Vec<(String, String)> = skipped.into_iter().collect();
    skipped.sort();
    Ok((module, skipped))
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
            module.verify().map_err(|e| {
                format!("LLVM module verification failed after adding entry: {e}")
            })?;
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

/// The LLVM function type for a MIR function. Aggregates are passed/returned as
/// `[size x i8]` byte storage (a self-consistent by-value ABI for la3↔la3
/// calls); a unit return is `void`. Only called for functions that passed
/// [`unsupported_reason`].
fn fn_type<'ctx>(
    ctx: &'ctx Context,
    f: &MirFn,
    oracle: &LayoutOracle,
) -> inkwell::types::FunctionType<'ctx> {
    let params: Vec<BasicMetadataTypeEnum> = (1..=f.arg_count)
        .map(|i| storage_ty(ctx, &f.locals[i].ty, oracle).unwrap().into())
        .collect();
    match storage_ty(ctx, &f.locals[0].ty, oracle) {
        Some(ret) => ret.fn_type(&params, false),
        None => ctx.void_type().fn_type(&params, false), // unit return
    }
}

/// Map a *scalar* La3 type to its LLVM type, or `None` for anything else.
fn scalar_ty<'ctx>(ctx: &'ctx Context, ty: &Ty) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
        Ty::Bool => Some(ctx.bool_type().into()),
        Ty::Char => Some(ctx.i32_type().into()),
        Ty::Int(k) => Some(int_ty(ctx, *k).into()),
        Ty::IntLit => Some(ctx.i32_type().into()),
        Ty::Float(FloatKind::F32) => Some(ctx.f32_type().into()),
        Ty::Float(FloatKind::F64) => Some(ctx.f64_type().into()),
        Ty::FloatLit => Some(ctx.f64_type().into()),
        _ => None,
    }
}

/// The LLVM *storage* type of a value: its scalar type, or — for an aggregate —
/// an `[size x i8]` blob laid out by the [`LayoutOracle`]. `None` for unit.
fn storage_ty<'ctx>(
    ctx: &'ctx Context,
    ty: &Ty,
    oracle: &LayoutOracle,
) -> Option<BasicTypeEnum<'ctx>> {
    if let Some(s) = scalar_ty(ctx, ty) {
        return Some(s);
    }
    if matches!(ty, Ty::Unit) {
        return None;
    }
    // Aggregate: opaque byte storage of the by-value size.
    let (size, _) = oracle.size_align(ty)?;
    Some(ctx.i8_type().array_type(size as u32).into())
}

fn int_ty<'ctx>(ctx: &'ctx Context, k: IntKind) -> IntType<'ctx> {
    match k {
        IntKind::I8 | IntKind::U8 => ctx.i8_type(),
        IntKind::I16 | IntKind::U16 => ctx.i16_type(),
        IntKind::I32 | IntKind::U32 => ctx.i32_type(),
        // isize/usize are 64-bit on the x86_64 v1 target.
        IntKind::I64 | IntKind::U64 | IntKind::Isize | IntKind::Usize => ctx.i64_type(),
    }
}

/// Reasons a function is out of current codegen scope (returns the first found).
/// Phase 5.4 adds **flat** aggregates (tuples/structs/enums whose fields are all
/// scalar) and their projections; nested aggregates, arrays, references, strings
/// and heap collections are still later phases.
fn unsupported_reason(f: &MirFn, oracle: &LayoutOracle) -> Option<String> {
    let tys = infer_local_types(f, oracle);
    for (i, ty) in tys.iter().enumerate() {
        if let Some(r) = ty_unsupported(ty, oracle) {
            return Some(format!("local _{i}: {r}"));
        }
    }
    for b in &f.blocks {
        for s in &b.stmts {
            if let Statement::Assign(p, rv) = s {
                if let Some(r) = proj_unsupported(&p.proj) {
                    return Some(r);
                }
                if let Some(r) = rvalue_unsupported(rv) {
                    return Some(r);
                }
                // A string literal or a global-value reference (`math.pi`, a free
                // fn used as a value) is not scalar codegen yet (Phase 6.1). Catch
                // it as an *operand* so the function is cleanly skipped rather than
                // hard-erroring deep in pass 3.
                for op in rvalue_operands(rv) {
                    if let Some(r) = const_operand_unsupported(op) {
                        return Some(r);
                    }
                }
            }
            // Drops of scalars/flat aggregates are no-ops here (no heap owned);
            // storage/nop carry no codegen.
        }
        match &b.term {
            Terminator::Return
            | Terminator::Goto(_)
            | Terminator::Unreachable
            | Terminator::If { .. }
            | Terminator::Switch { .. } => {}
            Terminator::Call { func, args, .. } => {
                if !matches!(func, Operand::Const(Const::Fn(_))) {
                    return Some("indirect call (closure/fn value) — Phase 8".into());
                }
                // Likewise reject a string/global-value *argument* up front.
                for op in args {
                    if let Some(r) = const_operand_unsupported(op) {
                        return Some(r);
                    }
                }
            }
        }
    }
    None
}

/// The operands an rvalue reads (so the skip-predicate can inspect them).
fn rvalue_operands(rv: &Rvalue) -> Vec<&Operand> {
    match rv {
        Rvalue::Use(o) | Rvalue::Unary(_, o) | Rvalue::Cast(o, _) => vec![o],
        Rvalue::Binary(_, a, b) => vec![a, b],
        Rvalue::Aggregate(_, ops) => ops.iter().collect(),
        Rvalue::Ref(_) | Rvalue::Discriminant(_) => vec![],
    }
}

/// A `str`/global-value constant used as a *value* operand is beyond scalar
/// codegen (Phase 6.1) — return why so the function is skipped. (`Const::Fn` in
/// a call's `func` position is fine; it is handled by the call lowering.)
fn const_operand_unsupported(op: &Operand) -> Option<String> {
    match op {
        Operand::Const(Const::Str(_)) => Some("string literal — Phase 6.1".into()),
        Operand::Const(Const::Fn(name)) => {
            Some(format!("global-value reference `{name}` — Phase 6.1"))
        }
        _ => None,
    }
}

fn is_scalar(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Bool | Ty::Char | Ty::Int(_) | Ty::IntLit | Ty::Float(_) | Ty::FloatLit
    )
}

/// `None` if `ty` is codegen-able here: a scalar, unit, or a **flat** aggregate
/// (tuple/struct/enum whose every field/payload is a scalar).
fn ty_unsupported(ty: &Ty, oracle: &LayoutOracle) -> Option<String> {
    if is_scalar(ty) || matches!(ty, Ty::Unit) {
        return None;
    }
    match ty {
        Ty::Tuple(_) | Ty::Struct(..) => match oracle.agg_fields(ty) {
            Some(fields) if fields.iter().all(|(_, t)| is_scalar(t)) => None,
            Some(_) => Some(format!(
                "nested aggregate {} — later phase",
                crate::ty::display_ty(ty)
            )),
            None => Some(format!("unsized/generic {}", crate::ty::display_ty(ty))),
        },
        Ty::Enum(..) => match oracle.enum_info(ty) {
            Some(info)
                if info
                    .variants
                    .iter()
                    .all(|v| v.iter().all(|(_, t)| is_scalar(t))) =>
            {
                None
            }
            Some(_) => Some(format!(
                "enum {} with non-scalar payload — later phase",
                crate::ty::display_ty(ty)
            )),
            None => Some(format!(
                "unsized/generic enum {}",
                crate::ty::display_ty(ty)
            )),
        },
        _ => Some(format!(
            "non-scalar type {} — later phase",
            crate::ty::display_ty(ty)
        )),
    }
}

/// Only `Field`/`Downcast` projections are codegen-able here (struct/tuple field
/// and enum-variant payload access); `Index`/`Deref` need arrays/refs (Phase 6).
fn proj_unsupported(proj: &[Projection]) -> Option<String> {
    for p in proj {
        match p {
            Projection::Field(_) | Projection::Downcast(_) => {}
            Projection::Index(_) => return Some("array index projection — Phase 6".into()),
            Projection::Deref => return Some("deref projection — Phase 6".into()),
        }
    }
    None
}

/// Rvalues still outside scope (references, arrays, closures).
fn rvalue_unsupported(rv: &Rvalue) -> Option<String> {
    match rv {
        Rvalue::Use(_)
        | Rvalue::Binary(..)
        | Rvalue::Unary(..)
        | Rvalue::Cast(..)
        | Rvalue::Discriminant(_) => None,
        Rvalue::Ref(_) => Some("reference rvalue — Phase 6".into()),
        Rvalue::Aggregate(AggregateKind::Array, _) => Some("array literal — Phase 6".into()),
        Rvalue::Aggregate(AggregateKind::Closure(_), _) => Some("closure value — Phase 8".into()),
        Rvalue::Aggregate(_, _) => None, // Tuple / Struct / Variant
    }
}

/// If `name` is an enum tuple-variant **constructor** (`Enum.Variant`, lowered
/// by mirgen as a call), return the enum name and the variant's index. Codegen
/// turns such a "call" into an aggregate construction.
fn enum_ctor(name: &str, oracle: &LayoutOracle) -> Option<(String, usize)> {
    let (ename, variant) = name.split_once('.')?;
    // Confirm it is a real enum (not a module function like `io.println`).
    oracle.enum_info(&Ty::Enum(ename.to_string(), Vec::new()))?;
    let idx = oracle.variant_index(ename, variant)?;
    Some((ename.to_string(), idx))
}

/// Resolve `Unknown`-typed temporaries to a concrete type by propagating from
/// their definitions (a small fixpoint). The lenient type checker leaves some
/// temporaries — notably `match`/arm result slots and variant-constructor call
/// destinations — typed `_`, even though the value flowing through them is
/// concrete; codegen needs a real type to lay out the slot.
fn infer_local_types(f: &MirFn, oracle: &LayoutOracle) -> Vec<Ty> {
    let mut tys: Vec<Ty> = f.locals.iter().map(|l| l.ty.clone()).collect();
    fn op_ty(op: &Operand, tys: &[Ty]) -> Option<Ty> {
        match op {
            Operand::Copy(p) | Operand::Move(p) if p.proj.is_empty() => {
                let t = &tys[p.local.0 as usize];
                (!matches!(t, Ty::Unknown)).then(|| t.clone())
            }
            Operand::Const(Const::Int(_, ty)) => Some(if matches!(ty, Ty::Int(_)) {
                ty.clone()
            } else {
                Ty::Int(IntKind::I32)
            }),
            Operand::Const(Const::Float(_)) => Some(Ty::Float(FloatKind::F64)),
            Operand::Const(Const::Bool(_)) => Some(Ty::Bool),
            Operand::Const(Const::Char(_)) => Some(Ty::Char),
            _ => None,
        }
    }
    loop {
        let mut changed = false;
        for b in &f.blocks {
            for s in &b.stmts {
                let Statement::Assign(place, rv) = s else {
                    continue;
                };
                if !place.proj.is_empty() || !matches!(tys[place.local.0 as usize], Ty::Unknown) {
                    continue;
                }
                let inferred = match rv {
                    Rvalue::Use(op) => op_ty(op, &tys),
                    Rvalue::Binary(op, a, b) => match op {
                        BinOp::Eq
                        | BinOp::Ne
                        | BinOp::Lt
                        | BinOp::Gt
                        | BinOp::Le
                        | BinOp::Ge
                        | BinOp::And
                        | BinOp::Or => Some(Ty::Bool),
                        BinOp::Pow => Some(Ty::Float(FloatKind::F64)),
                        _ => op_ty(a, &tys).or_else(|| op_ty(b, &tys)),
                    },
                    Rvalue::Unary(UnOp::Not, _) => Some(Ty::Bool),
                    Rvalue::Unary(_, a) => op_ty(a, &tys),
                    Rvalue::Cast(_, ty) => Some(ty.clone()),
                    Rvalue::Discriminant(_) => Some(Ty::Int(IntKind::I32)),
                    // A constructor pins the aggregate's nominal type (the
                    // checker may have left the temp `_`). Generic args are not
                    // recoverable here, but generic aggregates are out of scope.
                    Rvalue::Aggregate(AggregateKind::Struct(name), _) => {
                        Some(Ty::Struct(name.clone(), Vec::new()))
                    }
                    Rvalue::Aggregate(AggregateKind::Variant(name, _), _) => {
                        Some(Ty::Enum(name.clone(), Vec::new()))
                    }
                    Rvalue::Aggregate(AggregateKind::Tuple, ops) => ops
                        .iter()
                        .map(|o| op_ty(o, &tys))
                        .collect::<Option<Vec<_>>>()
                        .map(Ty::Tuple),
                    _ => None,
                };
                if let Some(t) = inferred {
                    if !matches!(t, Ty::Unknown) {
                        tys[place.local.0 as usize] = t;
                        changed = true;
                    }
                }
            }
            // A variant-constructor call's destination is the enum.
            if let Terminator::Call {
                func: Operand::Const(Const::Fn(name)),
                dest: Some((place, _)),
                ..
            } = &b.term
            {
                if place.proj.is_empty() && matches!(tys[place.local.0 as usize], Ty::Unknown) {
                    if let Some((ename, _)) = enum_ctor(name, oracle) {
                        tys[place.local.0 as usize] = Ty::Enum(ename, Vec::new());
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    tys
}

/// The `Const::Fn` symbols a function calls — excluding enum-variant
/// constructors, which codegen lowers inline (they are not real functions).
fn call_targets(f: &MirFn, oracle: &LayoutOracle) -> Vec<String> {
    let mut out = Vec::new();
    for b in &f.blocks {
        if let Terminator::Call {
            func: Operand::Const(Const::Fn(name)),
            ..
        } = &b.term
        {
            if enum_ctor(name, oracle).is_none() {
                out.push(name.clone());
            }
        }
    }
    out
}

/// Per-function translation state.
struct FnGen<'a, 'ctx> {
    ctx: &'ctx Context,
    module: &'a Module<'ctx>,
    oracle: &'a LayoutOracle,
    builder: Builder<'ctx>,
    decls: &'a HashMap<String, FunctionValue<'ctx>>,
    /// Each function symbol → its parameter types (to type call arguments).
    sigs: &'a HashMap<String, Vec<Ty>>,
    f: &'a MirFn,
    /// Per-local types with `Unknown` temporaries resolved (see
    /// [`infer_local_types`]).
    local_types: Vec<Ty>,
    fval: FunctionValue<'ctx>,
    /// One `alloca` per local (`None` for unit locals, which hold no value). A
    /// scalar local's slot points at its scalar type; an aggregate's at its
    /// `[size x i8]` byte storage.
    slots: Vec<Option<PointerValue<'ctx>>>,
    /// One LLVM block per MIR block.
    blocks: Vec<inkwell::basic_block::BasicBlock<'ctx>>,
}

impl<'a, 'ctx> FnGen<'a, 'ctx> {
    /// The (inference-resolved) type of a local.
    fn lty(&self, l: crate::mir::Local) -> &Ty {
        &self.local_types[l.0 as usize]
    }

    fn gen_fn(&mut self) -> Result<(), String> {
        // Entry block: stack slots for every local, then the params stored in.
        let entry = self.ctx.append_basic_block(self.fval, "entry");
        self.builder.position_at_end(entry);
        for ty in self.local_types.clone() {
            let slot = match storage_ty(self.ctx, &ty, self.oracle) {
                Some(t) => {
                    let ptr = self.b(self.builder.build_alloca(t, "slot"))?;
                    // Aggregate storage is `[N x i8]` (align 1 by type), so set the
                    // alloca's alignment to the value's real alignment.
                    if !is_scalar(&ty) {
                        if let Some((_, align)) = self.oracle.size_align(&ty) {
                            if let Some(inst) = ptr.as_instruction() {
                                let _ = inst.set_alignment(align.max(1) as u32);
                            }
                        }
                    }
                    Some(ptr)
                }
                None => None,
            };
            self.slots.push(slot);
        }
        for i in 1..=self.f.arg_count {
            if let Some(slot) = self.slots[i] {
                let p = self.fval.get_nth_param((i - 1) as u32).unwrap();
                self.b(self.builder.build_store(slot, p))?;
            }
        }

        for i in 0..self.f.blocks.len() {
            self.blocks
                .push(self.ctx.append_basic_block(self.fval, &format!("bb{i}")));
        }
        self.b(self.builder.build_unconditional_branch(self.blocks[0]))?;

        for (i, blk) in self.f.blocks.iter().enumerate() {
            self.builder.position_at_end(self.blocks[i]);
            self.gen_block(blk)?;
        }
        Ok(())
    }

    fn gen_block(&mut self, blk: &BasicBlock) -> Result<(), String> {
        for s in &blk.stmts {
            if let Statement::Assign(place, rv) = s {
                self.gen_assign(place, rv)?;
            }
            // Drop / Storage* / Nop have no codegen (flat values own no heap yet).
        }
        self.gen_term(&blk.term)
    }

    /// `place = rvalue`, dispatching on whether the destination is a scalar or an
    /// aggregate (built/copied through its byte storage).
    fn gen_assign(&mut self, place: &Place, rv: &Rvalue) -> Result<(), String> {
        // A unit-typed destination has no storage; rvalues are side-effect-free
        // (calls are terminators), so the assignment is a no-op.
        if place.proj.is_empty() && self.slots[place.local.0 as usize].is_none() {
            return Ok(());
        }
        let (dptr, dty) = self.resolve_place(place)?;
        match rv {
            // Build a tuple/struct/enum value directly into the destination.
            Rvalue::Aggregate(kind, ops) => self.gen_aggregate(dptr, &dty, kind, ops),
            // Enum tag → an integer scalar (zero-extended to the dest width).
            Rvalue::Discriminant(ep) => {
                let v = self.gen_discriminant(ep, &dty)?;
                self.b(self.builder.build_store(dptr, v))?;
                Ok(())
            }
            // A whole-aggregate move/copy is a byte copy of the storage.
            Rvalue::Use(op) if !is_scalar(&dty) => self.gen_aggregate_copy(dptr, &dty, op),
            // Everything else yields a scalar.
            _ => {
                let v = self.gen_scalar_rvalue(rv, &dty)?;
                self.b(self.builder.build_store(dptr, v))?;
                Ok(())
            }
        }
    }

    /// Resolve a [`Place`] to the pointer at its leaf and the leaf's type,
    /// walking `Field`/`Downcast` projections as byte offsets (the layout the
    /// oracle computes; same as the by-value layout the rest of the compiler
    /// uses).
    fn resolve_place(&self, place: &Place) -> Result<(PointerValue<'ctx>, Ty), String> {
        let base = self.slots[place.local.0 as usize].ok_or("place rooted at a unit local")?;
        let mut cur_ty = self.lty(place.local).clone();
        let mut offset: u64 = 0;
        // After a `Downcast`, field indices address the chosen variant's payload.
        let mut payload: Option<Vec<(u64, Ty)>> = None;
        for p in &place.proj {
            match p {
                Projection::Downcast(v) => {
                    let info = self
                        .oracle
                        .enum_info(&cur_ty)
                        .ok_or("downcast on a non-enum")?;
                    offset += info.payload_offset;
                    payload = Some(
                        info.variants
                            .get(*v)
                            .cloned()
                            .ok_or("variant index out of range")?,
                    );
                }
                Projection::Field(i) => {
                    if let Some(fields) = payload.take() {
                        let (foff, fty) = fields
                            .get(*i)
                            .cloned()
                            .ok_or("payload field index out of range")?;
                        offset += foff;
                        cur_ty = fty;
                    } else {
                        let fields = self
                            .oracle
                            .agg_fields(&cur_ty)
                            .ok_or("field access on a non-aggregate")?;
                        let (foff, fty) =
                            fields.get(*i).cloned().ok_or("field index out of range")?;
                        offset += foff;
                        cur_ty = fty;
                    }
                }
                Projection::Index(_) | Projection::Deref => {
                    return Err("index/deref projection survived to codegen".into());
                }
            }
        }
        let ptr = if offset == 0 {
            base
        } else {
            let i8t = self.ctx.i8_type();
            let idx = self.ctx.i64_type().const_int(offset, false);
            self.b(unsafe { self.builder.build_in_bounds_gep(i8t, base, &[idx], "field") })?
        };
        Ok((ptr, cur_ty))
    }

    /// Construct a tuple/struct/enum value into `dptr` (the destination storage).
    fn gen_aggregate(
        &mut self,
        dptr: PointerValue<'ctx>,
        dty: &Ty,
        kind: &AggregateKind,
        ops: &[Operand],
    ) -> Result<(), String> {
        match kind {
            AggregateKind::Tuple | AggregateKind::Struct(_) => {
                let fields = self
                    .oracle
                    .agg_fields(dty)
                    .ok_or("aggregate construction of a non-aggregate")?;
                for (op, (off, fty)) in ops.iter().zip(fields) {
                    self.store_scalar_at(dptr, off, op, &fty)?;
                }
                Ok(())
            }
            AggregateKind::Variant(_, vidx) => {
                let info = self.oracle.enum_info(dty).ok_or("variant of a non-enum")?;
                // Store the discriminant tag at offset 0.
                let tag_ty = self.int_of_bytes(info.tag_size);
                let tagv = tag_ty.const_int(*vidx as u64, false);
                self.b(self.builder.build_store(dptr, tagv))?;
                // Store each payload field at payload_offset + its offset.
                let var = info
                    .variants
                    .get(*vidx)
                    .cloned()
                    .ok_or("variant index out of range")?;
                for (op, (off, fty)) in ops.iter().zip(var) {
                    self.store_scalar_at(dptr, info.payload_offset + off, op, &fty)?;
                }
                Ok(())
            }
            AggregateKind::Array | AggregateKind::Closure(_) => {
                Err("array/closure aggregate survived to codegen".into())
            }
        }
    }

    /// Store a scalar operand `op` (of type `fty`) at byte `offset` from `base`.
    fn store_scalar_at(
        &mut self,
        base: PointerValue<'ctx>,
        offset: u64,
        op: &Operand,
        fty: &Ty,
    ) -> Result<(), String> {
        let v = self
            .gen_operand(op, fty)?
            .ok_or("unit value stored into an aggregate field")?;
        let ptr = self.gep_byte(base, offset)?;
        self.b(self.builder.build_store(ptr, v))?;
        Ok(())
    }

    /// Copy a whole aggregate value (a `move`/`copy` of one place into another).
    fn gen_aggregate_copy(
        &mut self,
        dptr: PointerValue<'ctx>,
        dty: &Ty,
        op: &Operand,
    ) -> Result<(), String> {
        let src = match op {
            Operand::Copy(p) | Operand::Move(p) => self.resolve_place(p)?.0,
            Operand::Const(_) => return Err("aggregate constant is not supported".into()),
        };
        let (size, align) = self
            .oracle
            .size_align(dty)
            .ok_or("aggregate of unknown size")?;
        let n = self.ctx.i64_type().const_int(size, false);
        self.b(self
            .builder
            .build_memcpy(dptr, align.max(1) as u32, src, align.max(1) as u32, n))
            .map(|_| ())
    }

    /// Read an enum's discriminant tag and zero-extend it to `dty` (an integer).
    fn gen_discriminant(&mut self, ep: &Place, dty: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
        let (eptr, ety) = self.resolve_place(ep)?;
        let info = self
            .oracle
            .enum_info(&ety)
            .ok_or("discriminant of a non-enum")?;
        let tag_ty = self.int_of_bytes(info.tag_size);
        // The tag is at offset 0 of the enum storage.
        let raw = self
            .b(self.builder.build_load(tag_ty, eptr, "tag"))?
            .into_int_value();
        let dest_ty = scalar_ty(self.ctx, dty)
            .ok_or("discriminant destination is not a scalar")?
            .into_int_type();
        Ok(self
            .b(self.builder.build_int_z_extend(raw, dest_ty, "tagext"))?
            .into())
    }

    /// An LLVM integer type of `bytes` bytes (1/2/4) for an enum tag.
    fn int_of_bytes(&self, bytes: u64) -> IntType<'ctx> {
        match bytes {
            1 => self.ctx.i8_type(),
            2 => self.ctx.i16_type(),
            _ => self.ctx.i32_type(),
        }
    }

    /// GEP `base` (treated as `i8*`) by `offset` bytes.
    fn gep_byte(
        &self,
        base: PointerValue<'ctx>,
        offset: u64,
    ) -> Result<PointerValue<'ctx>, String> {
        if offset == 0 {
            return Ok(base);
        }
        let i8t = self.ctx.i8_type();
        let idx = self.ctx.i64_type().const_int(offset, false);
        self.b(unsafe { self.builder.build_in_bounds_gep(i8t, base, &[idx], "off") })
    }

    fn gen_term(&mut self, term: &Terminator) -> Result<(), String> {
        match term {
            Terminator::Return => {
                match self.slots[0] {
                    Some(slot) => {
                        // `storage_ty` covers both scalar and aggregate (`[N x i8]`)
                        // returns.
                        let ty = storage_ty(self.ctx, &self.local_types[0].clone(), self.oracle)
                            .unwrap();
                        let v = self.b(self.builder.build_load(ty, slot, "ret"))?;
                        self.b(self.builder.build_return(Some(&v)))?;
                    }
                    None => {
                        self.b(self.builder.build_return(None))?;
                    }
                }
            }
            Terminator::Goto(b) => {
                self.b(self
                    .builder
                    .build_unconditional_branch(self.blocks[b.0 as usize]))?;
            }
            Terminator::Unreachable => {
                self.b(self.builder.build_unreachable())?;
            }
            Terminator::Call { func, args, dest } => {
                let name = match func {
                    Operand::Const(Const::Fn(n)) => n,
                    _ => return Err("indirect call survived to codegen".into()),
                };
                // An enum tuple-variant "constructor" (`Enum.Variant`) is lowered
                // by mirgen as a call; build the variant aggregate inline instead.
                if let Some((_, vidx)) = enum_ctor(name, self.oracle) {
                    if let Some((place, next)) = dest {
                        let (dptr, dty) = self.resolve_place(place)?;
                        self.gen_aggregate(
                            dptr,
                            &dty,
                            &AggregateKind::Variant(String::new(), vidx),
                            args,
                        )?;
                        self.b(self
                            .builder
                            .build_unconditional_branch(self.blocks[next.0 as usize]))?;
                    }
                    return Ok(());
                }
                let callee = *self
                    .decls
                    .get(name)
                    .ok_or_else(|| format!("call to undeclared fn `{name}`"))?;
                // Argument operands, each at its callee parameter type (so a
                // constant gets the right width and an aggregate is passed whole).
                let param_tys = self.sigs.get(name).cloned().unwrap_or_default();
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    let expect = param_tys.get(i).cloned().unwrap_or(Ty::Unknown);
                    let v = self
                        .gen_operand_full(a, &expect)?
                        .ok_or("unit-typed call argument")?;
                    argv.push(v.into());
                }
                let call = self.b(self.builder.build_call(callee, &argv, "call"))?;
                if let Some((place, next)) = dest {
                    if let Some(v) = call.try_as_basic_value().basic() {
                        // Store the result (scalar or `[N x i8]`) at the dest place.
                        let (dptr, _) = self.resolve_place(place)?;
                        self.b(self.builder.build_store(dptr, v))?;
                    }
                    self.b(self
                        .builder
                        .build_unconditional_branch(self.blocks[next.0 as usize]))?;
                } else {
                    self.b(self.builder.build_unreachable())?;
                }
            }
            Terminator::If {
                cond,
                then_blk,
                else_blk,
            } => {
                let c = self
                    .gen_operand(cond, &Ty::Bool)?
                    .ok_or("unit operand as `if` condition")?;
                self.b(self.builder.build_conditional_branch(
                    c.into_int_value(),
                    self.blocks[then_blk.0 as usize],
                    self.blocks[else_blk.0 as usize],
                ))?;
            }
            Terminator::Switch {
                discr,
                targets,
                default,
            } => {
                // The discriminant is an integer/char/bool value (enum-discriminant
                // switches go through `Rvalue::Discriminant`, still Phase 5.4). Each
                // arm value is a constant of the discriminant's type.
                let dty = self.operand_ty(discr).unwrap_or(Ty::Int(IntKind::I64));
                let dv = self
                    .gen_operand(discr, &dty)?
                    .ok_or("unit operand as switch discriminant")?
                    .into_int_value();
                let int_ty = dv.get_type();
                let cases: Vec<_> = targets
                    .iter()
                    .map(|(v, b)| {
                        (
                            int_ty.const_int(*v as i64 as u64, false),
                            self.blocks[b.0 as usize],
                        )
                    })
                    .collect();
                self.b(self
                    .builder
                    .build_switch(dv, self.blocks[default.0 as usize], &cases))?;
            }
        }
        Ok(())
    }

    // -- rvalues / operands ------------------------------------------------

    /// Compute a **scalar** rvalue's value (the only rvalues that reach here;
    /// aggregate/discriminant rvalues are handled in [`Self::gen_assign`]).
    fn gen_scalar_rvalue(
        &mut self,
        rv: &Rvalue,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match rv {
            Rvalue::Use(op) => self
                .gen_operand(op, expected)?
                .ok_or_else(|| "unit value in a scalar assignment".into()),
            Rvalue::Binary(op, a, b) => self.gen_binary(*op, a, b, expected),
            Rvalue::Unary(op, a) => self.gen_unary(*op, a, expected),
            Rvalue::Cast(op, ty) => self.gen_cast(op, ty),
            _ => Err("non-scalar rvalue reached scalar lowering".into()),
        }
    }

    /// Load a **scalar** operand (a scalar place leaf or a constant); `None` for
    /// a unit value.
    fn gen_operand(
        &mut self,
        op: &Operand,
        expected: &Ty,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                // A bare unit local holds no value.
                if p.proj.is_empty() && self.slots[p.local.0 as usize].is_none() {
                    return Ok(None);
                }
                let (ptr, lty) = self.resolve_place(p)?;
                let st =
                    scalar_ty(self.ctx, &lty).ok_or("non-scalar operand in a scalar context")?;
                Ok(Some(self.b(self.builder.build_load(st, ptr, "load"))?))
            }
            Operand::Const(c) => self.gen_const(c, expected),
        }
    }

    /// Load an operand's full value — scalar *or* aggregate (`[N x i8]`) — for a
    /// call argument or a return. `None` for unit.
    fn gen_operand_full(
        &mut self,
        op: &Operand,
        ty: &Ty,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        if is_scalar(ty) || matches!(ty, Ty::Unit) {
            return self.gen_operand(op, ty);
        }
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (ptr, _) = self.resolve_place(p)?;
                let st = storage_ty(self.ctx, ty, self.oracle)
                    .ok_or("aggregate operand of unknown layout")?;
                Ok(Some(self.b(self.builder.build_load(st, ptr, "aload"))?))
            }
            Operand::Const(_) => Err("aggregate constant operand is unsupported".into()),
        }
    }

    fn gen_const(&self, c: &Const, expected: &Ty) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let v = match c {
            Const::Int(v, ty) => {
                // Prefer the contextual width (`expected`) — typeck pinned the
                // literal to it — falling back to the const's own kind.
                let it = match expected {
                    Ty::Int(k) => int_ty(self.ctx, *k),
                    _ => match ty {
                        Ty::Int(k) => int_ty(self.ctx, *k),
                        _ => self.ctx.i32_type(),
                    },
                };
                it.const_int(*v as u64, false).into()
            }
            Const::Float(f) => {
                let ft = match expected {
                    Ty::Float(FloatKind::F32) => self.ctx.f32_type(),
                    _ => self.ctx.f64_type(),
                };
                ft.const_float(*f).into()
            }
            Const::Bool(b) => self.ctx.bool_type().const_int(*b as u64, false).into(),
            Const::Char(ch) => self.ctx.i32_type().const_int(*ch as u64, false).into(),
            Const::Unit | Const::Nil => return Ok(None),
            Const::Str(_) | Const::Fn(_) => {
                return Err("string/fn constant is not a scalar value (Phase 5.4+)".into());
            }
        };
        Ok(Some(v))
    }

    // -- binary / unary / cast --------------------------------------------

    fn gen_binary(
        &mut self,
        op: BinOp,
        a: &Operand,
        b: &Operand,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use BinOp::*;
        // Comparisons and logical connectives produce a bool from operands of
        // their own type; everything else produces a value of `expected`.
        match op {
            Eq | Ne | Lt | Gt | Le | Ge => {
                let ot = self
                    .operand_ty(a)
                    .or_else(|| self.operand_ty(b))
                    .unwrap_or_else(|| {
                        if self.is_float_const(a) || self.is_float_const(b) {
                            Ty::Float(FloatKind::F64)
                        } else {
                            Ty::Int(IntKind::I32)
                        }
                    });
                let la = self
                    .gen_operand(a, &ot)?
                    .ok_or("unit operand in comparison")?;
                let lb = self
                    .gen_operand(b, &ot)?
                    .ok_or("unit operand in comparison")?;
                self.gen_compare(op, la, lb, &ot)
            }
            And | Or => {
                let la = self
                    .gen_operand(a, &Ty::Bool)?
                    .ok_or("unit operand in `&&`/`||`")?;
                let lb = self
                    .gen_operand(b, &Ty::Bool)?
                    .ok_or("unit operand in `&&`/`||`")?;
                let (x, y) = (la.into_int_value(), lb.into_int_value());
                let r = if op == And {
                    self.b(self.builder.build_and(x, y, "and"))?
                } else {
                    self.b(self.builder.build_or(x, y, "or"))?
                };
                Ok(r.into())
            }
            Pow => {
                // `**` always yields f64 (reference §4); convert both operands.
                let fa = self.to_f64(a)?;
                let fb = self.to_f64(b)?;
                let f64t = self.ctx.f64_type();
                let powf = self.decls_pow(f64t);
                let call = self.b(self
                    .builder
                    .build_call(powf, &[fa.into(), fb.into()], "pow"))?;
                Ok(call
                    .try_as_basic_value()
                    .basic()
                    .ok_or("pow returned void")?)
            }
            _ => {
                // Arithmetic / bitwise, computed at `expected`.
                let la = self
                    .gen_operand(a, expected)?
                    .ok_or("unit operand in arithmetic")?;
                let lb = self
                    .gen_operand(b, expected)?
                    .ok_or("unit operand in arithmetic")?;
                match expected {
                    Ty::Float(_) | Ty::FloatLit => {
                        let (x, y) = (la.into_float_value(), lb.into_float_value());
                        let r = match op {
                            Add => self.b(self.builder.build_float_add(x, y, "fadd"))?,
                            Sub => self.b(self.builder.build_float_sub(x, y, "fsub"))?,
                            Mul => self.b(self.builder.build_float_mul(x, y, "fmul"))?,
                            Div => self.b(self.builder.build_float_div(x, y, "fdiv"))?,
                            Rem => self.b(self.builder.build_float_rem(x, y, "frem"))?,
                            _ => return Err(format!("operator {op:?} is not valid on floats")),
                        };
                        Ok(r.into())
                    }
                    _ => {
                        let signed = match expected {
                            Ty::Int(k) => is_signed(*k),
                            _ => true, // IntLit → i32 signed
                        };
                        let (x, y) = (la.into_int_value(), lb.into_int_value());
                        let r = match op {
                            Add => self.b(self.builder.build_int_add(x, y, "add"))?,
                            Sub => self.b(self.builder.build_int_sub(x, y, "sub"))?,
                            Mul => self.b(self.builder.build_int_mul(x, y, "mul"))?,
                            Div if signed => {
                                self.b(self.builder.build_int_signed_div(x, y, "sdiv"))?
                            }
                            Div => self.b(self.builder.build_int_unsigned_div(x, y, "udiv"))?,
                            Rem if signed => {
                                self.b(self.builder.build_int_signed_rem(x, y, "srem"))?
                            }
                            Rem => self.b(self.builder.build_int_unsigned_rem(x, y, "urem"))?,
                            BitAnd => self.b(self.builder.build_and(x, y, "band"))?,
                            BitOr => self.b(self.builder.build_or(x, y, "bor"))?,
                            BitXor => self.b(self.builder.build_xor(x, y, "bxor"))?,
                            Shl => self.b(self.builder.build_left_shift(x, y, "shl"))?,
                            Shr => self.b(self.builder.build_right_shift(x, y, signed, "shr"))?,
                            _ => return Err(format!("operator {op:?} is not valid on integers")),
                        };
                        Ok(r.into())
                    }
                }
            }
        }
    }

    fn gen_compare(
        &mut self,
        op: BinOp,
        la: BasicValueEnum<'ctx>,
        lb: BasicValueEnum<'ctx>,
        ot: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use BinOp::*;
        let r = match ot {
            Ty::Float(_) | Ty::FloatLit => {
                let pred = match op {
                    Eq => FloatPredicate::OEQ,
                    Ne => FloatPredicate::UNE,
                    Lt => FloatPredicate::OLT,
                    Gt => FloatPredicate::OGT,
                    Le => FloatPredicate::OLE,
                    Ge => FloatPredicate::OGE,
                    _ => unreachable!(),
                };
                self.b(self.builder.build_float_compare(
                    pred,
                    la.into_float_value(),
                    lb.into_float_value(),
                    "fcmp",
                ))?
            }
            _ => {
                let signed = match ot {
                    Ty::Int(k) => is_signed(*k),
                    _ => true,
                };
                let pred = match (op, signed) {
                    (Eq, _) => IntPredicate::EQ,
                    (Ne, _) => IntPredicate::NE,
                    (Lt, true) => IntPredicate::SLT,
                    (Lt, false) => IntPredicate::ULT,
                    (Gt, true) => IntPredicate::SGT,
                    (Gt, false) => IntPredicate::UGT,
                    (Le, true) => IntPredicate::SLE,
                    (Le, false) => IntPredicate::ULE,
                    (Ge, true) => IntPredicate::SGE,
                    (Ge, false) => IntPredicate::UGE,
                    _ => unreachable!(),
                };
                self.b(self.builder.build_int_compare(
                    pred,
                    la.into_int_value(),
                    lb.into_int_value(),
                    "icmp",
                ))?
            }
        };
        Ok(r.into())
    }

    fn gen_unary(
        &mut self,
        op: UnOp,
        a: &Operand,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self
            .gen_operand(a, expected)?
            .ok_or("unit operand in unary op")?;
        let r: BasicValueEnum = match op {
            UnOp::Neg => match expected {
                Ty::Float(_) | Ty::FloatLit => self
                    .b(self.builder.build_float_neg(v.into_float_value(), "fneg"))?
                    .into(),
                _ => self
                    .b(self.builder.build_int_neg(v.into_int_value(), "neg"))?
                    .into(),
            },
            // Logical not on a bool and bitwise complement on an int are both
            // LLVM `not` (xor with all-ones / with 1).
            UnOp::Not | UnOp::BitNot => self
                .b(self.builder.build_not(v.into_int_value(), "not"))?
                .into(),
            UnOp::Deref | UnOp::Ref | UnOp::RefMut | UnOp::RawRef => {
                return Err("ref/deref unary survived to codegen (Phase 6)".into());
            }
        };
        Ok(r)
    }

    fn gen_cast(&mut self, op: &Operand, target: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Int(IntKind::I32));
        let v = self
            .gen_operand(op, &src_ty)?
            .ok_or("unit operand in cast")?;
        let src_int = matches!(src_ty, Ty::Int(_) | Ty::IntLit | Ty::Char);
        let src_signed = match src_ty {
            Ty::Int(k) => is_signed(k),
            Ty::Char => false, // a Unicode scalar is a non-negative codepoint
            _ => true,
        };
        let r: BasicValueEnum = match target {
            Ty::Int(_) | Ty::Char => {
                let tt = scalar_ty(self.ctx, target).unwrap().into_int_type();
                if src_int {
                    self.b(self.builder.build_int_cast_sign_flag(
                        v.into_int_value(),
                        tt,
                        src_signed,
                        "icast",
                    ))?
                    .into()
                } else {
                    // float → int truncates toward zero (matches the oracle).
                    let signed_target = match target {
                        Ty::Int(k) => is_signed(*k),
                        _ => false, // char
                    };
                    if signed_target {
                        self.b(self.builder.build_float_to_signed_int(
                            v.into_float_value(),
                            tt,
                            "fptosi",
                        ))?
                        .into()
                    } else {
                        self.b(self.builder.build_float_to_unsigned_int(
                            v.into_float_value(),
                            tt,
                            "fptoui",
                        ))?
                        .into()
                    }
                }
            }
            Ty::Float(_) | Ty::FloatLit => {
                let tt = scalar_ty(self.ctx, target).unwrap().into_float_type();
                if src_int {
                    if src_signed {
                        self.b(self.builder.build_signed_int_to_float(
                            v.into_int_value(),
                            tt,
                            "sitofp",
                        ))?
                        .into()
                    } else {
                        self.b(self.builder.build_unsigned_int_to_float(
                            v.into_int_value(),
                            tt,
                            "uitofp",
                        ))?
                        .into()
                    }
                } else {
                    self.b(self
                        .builder
                        .build_float_cast(v.into_float_value(), tt, "fpcast"))?
                        .into()
                }
            }
            _ => {
                return Err(format!(
                    "cast to {} is not a scalar (Phase 5.4+)",
                    crate::ty::display_ty(target)
                ));
            }
        };
        Ok(r)
    }

    /// Convert an operand to `f64` (for `**`): int→float or float→f64.
    fn to_f64(&mut self, op: &Operand) -> Result<FloatValue<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Float(FloatKind::F64));
        let v = self
            .gen_operand(op, &src_ty)?
            .ok_or("unit operand in `**`")?;
        let f64t = self.ctx.f64_type();
        let r = match src_ty {
            Ty::Float(_) | Ty::FloatLit => {
                self.b(self
                    .builder
                    .build_float_cast(v.into_float_value(), f64t, "topf"))?
            }
            Ty::Int(k) if is_signed(k) => {
                self.b(self
                    .builder
                    .build_signed_int_to_float(v.into_int_value(), f64t, "topf"))?
            }
            _ => {
                self.b(self
                    .builder
                    .build_unsigned_int_to_float(v.into_int_value(), f64t, "topf"))?
            }
        };
        Ok(r)
    }

    /// Declare (once) `double @pow(double, double)` (libm, linked via `-lm`).
    fn decls_pow(&self, f64t: inkwell::types::FloatType<'ctx>) -> FunctionValue<'ctx> {
        match self.module.get_function("pow") {
            Some(f) => f,
            None => {
                let ty = f64t.fn_type(&[f64t.into(), f64t.into()], false);
                self.module.add_function("pow", ty, None)
            }
        }
    }

    /// The static type of an operand, when known (a place's local type or a
    /// typed constant). `None` for an untyped float constant.
    fn operand_ty(&self, op: &Operand) -> Option<Ty> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.place_ty(p),
            Operand::Const(Const::Int(_, ty)) => Some(match ty {
                Ty::Int(_) => ty.clone(),
                _ => Ty::Int(IntKind::I32),
            }),
            Operand::Const(Const::Bool(_)) => Some(Ty::Bool),
            Operand::Const(Const::Char(_)) => Some(Ty::Char),
            _ => None,
        }
    }

    fn is_float_const(&self, op: &Operand) -> bool {
        matches!(op, Operand::Const(Const::Float(_)))
    }

    /// The leaf type of a place, walking `Field`/`Downcast` projections — the
    /// type-only counterpart of [`Self::resolve_place`] (no IR emitted).
    fn place_ty(&self, place: &Place) -> Option<Ty> {
        let mut cur = self.lty(place.local).clone();
        let mut payload: Option<Vec<(u64, Ty)>> = None;
        for p in &place.proj {
            match p {
                Projection::Downcast(v) => {
                    payload = Some(self.oracle.enum_info(&cur)?.variants.get(*v)?.clone());
                }
                Projection::Field(i) => {
                    cur = match payload.take() {
                        Some(fields) => fields.get(*i)?.1.clone(),
                        None => self.oracle.agg_fields(&cur)?.get(*i)?.1.clone(),
                    };
                }
                Projection::Index(_) | Projection::Deref => return None,
            }
        }
        Some(cur)
    }

    /// Map a builder `Result` error to a `String`.
    fn b<T>(&self, r: Result<T, inkwell::builder::BuilderError>) -> Result<T, String> {
        r.map_err(|e| format!("LLVM builder error: {e}"))
    }
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
    fn jit<'ctx>(
        ctx: &'ctx Context,
        src: &str,
    ) -> inkwell::execution_engine::ExecutionEngine<'ctx> {
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
            let f: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
                ee.get_function("f").unwrap();
            assert_eq!(f.call(6, 7), -58);
            let d: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
                ee.get_function("d").unwrap();
            // `/` truncates toward zero (oracle: `a / b`).
            assert_eq!(d.call(-7, 2), -3);
            let r: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
                ee.get_function("r").unwrap();
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
            let ud: JitFunction<unsafe extern "C" fn(u32, u32) -> u32> =
                ee.get_function("ud").unwrap();
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
            let g: JitFunction<unsafe extern "C" fn(f64, f64) -> f64> =
                ee.get_function("g").unwrap();
            assert_eq!(g.call(1.0, 4.0), 1.25);
            let p: JitFunction<unsafe extern "C" fn(f64, f64) -> f64> =
                ee.get_function("p").unwrap();
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
            let sh: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
                ee.get_function("sh").unwrap();
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
        // A `str`-returning function is still beyond scalar codegen (Phase 5.4+),
        // so it must be reported skipped rather than mis-translated.
        let ctx = Context::create();
        let (mir, oracle) = lower_to_mir("fn s() -> str { \"hi\" }\nfn main() {}");
        let (_module, skipped) = build_program_module(&ctx, &mir, &oracle).expect("build module");
        assert!(
            skipped.iter().any(|(sym, _)| sym == "s"),
            "str-returning fn `s` reported skipped: {skipped:?}"
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
            let fib: JitFunction<unsafe extern "C" fn(i64) -> i64> =
                ee.get_function("fib").unwrap();
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
            let sum: JitFunction<unsafe extern "C" fn(i32) -> i32> =
                ee.get_function("sum").unwrap();
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
            let c: JitFunction<unsafe extern "C" fn(i32) -> i32> =
                ee.get_function("classify").unwrap();
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
            let st: JitFunction<unsafe extern "C" fn(i32, i32) -> i32> =
                ee.get_function("st").unwrap();
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
}

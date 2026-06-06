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
    BasicBlock, Const, MirFn, MirProgram, Operand, Rvalue, Statement, Terminator,
};
use crate::ty::{FloatKind, IntKind, Ty};

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
/// list of `(symbol, reason)` for functions Phase 5.2 cannot translate yet.
pub fn build_program_module<'ctx>(
    ctx: &'ctx Context,
    prog: &MirProgram,
) -> Result<(Module<'ctx>, Vec<(String, String)>), String> {
    let module = ctx.create_module("la3");

    // Pass 1: which functions can we translate? A function is supported only if
    // its own shape is supported *and* every function it calls is too (else the
    // call would reference an undefined symbol). Compute that as a fixpoint.
    let mut skipped: HashMap<String, String> = HashMap::new();
    let known: std::collections::HashSet<String> = prog.fns.iter().map(fn_symbol).collect();
    for f in &prog.fns {
        if let Some(reason) = unsupported_reason(f) {
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
            for callee in call_targets(f) {
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

    // Pass 2: declare the signatures of every supported function.
    let mut decls: HashMap<String, FunctionValue> = HashMap::new();
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if skipped.contains_key(&sym) {
            continue;
        }
        let fn_ty = fn_type(ctx, f);
        decls.insert(sym.clone(), module.add_function(&sym, fn_ty, None));
    }

    // Pass 3: translate the bodies.
    for f in &prog.fns {
        let sym = fn_symbol(f);
        if let Some(&fval) = decls.get(&sym) {
            let mut g = FnGen {
                ctx,
                module: &module,
                builder: ctx.create_builder(),
                decls: &decls,
                f,
                fval,
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

/// The LLVM function type for a MIR function (scalar params; `void` for a unit
/// return). Only called for functions that passed [`unsupported_reason`].
fn fn_type<'ctx>(ctx: &'ctx Context, f: &MirFn) -> inkwell::types::FunctionType<'ctx> {
    let params: Vec<BasicMetadataTypeEnum> = (1..=f.arg_count)
        .map(|i| scalar_ty(ctx, &f.locals[i].ty).unwrap().into())
        .collect();
    match scalar_ty(ctx, &f.locals[0].ty) {
        Some(ret) => ret.fn_type(&params, false),
        None => ctx.void_type().fn_type(&params, false), // unit return
    }
}

/// Map a *scalar* La3 type to its LLVM type, or `None` for unit / non-scalar
/// (the latter is rejected up front by [`unsupported_reason`]).
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

fn int_ty<'ctx>(ctx: &'ctx Context, k: IntKind) -> IntType<'ctx> {
    match k {
        IntKind::I8 | IntKind::U8 => ctx.i8_type(),
        IntKind::I16 | IntKind::U16 => ctx.i16_type(),
        IntKind::I32 | IntKind::U32 => ctx.i32_type(),
        // isize/usize are 64-bit on the x86_64 v1 target.
        IntKind::I64 | IntKind::U64 | IntKind::Isize | IntKind::Usize => ctx.i64_type(),
    }
}

/// Reasons a function is out of Phase 5.2 scope (returns the first found).
fn unsupported_reason(f: &MirFn) -> Option<String> {
    // Every local must be a scalar or unit (no aggregates, refs, strings, …).
    for (i, l) in f.locals.iter().enumerate() {
        if scalar_ty_is_none_and_not_unit(&l.ty) {
            return Some(format!("local _{i} has non-scalar type {}", crate::ty::display_ty(&l.ty)));
        }
    }
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Statement::Assign(p, rv) => {
                    if !p.proj.is_empty() {
                        return Some("place projection (field/index/deref) — Phase 5.4/6".into());
                    }
                    if let Some(r) = rvalue_unsupported(rv) {
                        return Some(r);
                    }
                }
                // Drops of scalars are no-ops; storage/nop carry no codegen.
                Statement::Drop(_) | Statement::StorageLive(_) | Statement::StorageDead(_)
                | Statement::Nop => {}
            }
        }
        match &b.term {
            // `If`/`Switch` are the MIR CFG's branches (Phase 5.3); `Goto` and
            // `Return` close blocks; `Unreachable` ends a diverging path.
            Terminator::Return
            | Terminator::Goto(_)
            | Terminator::Unreachable
            | Terminator::If { .. }
            | Terminator::Switch { .. } => {}
            Terminator::Call { func, .. } => {
                if !matches!(func, Operand::Const(Const::Fn(_))) {
                    return Some("indirect call (closure/fn value) — Phase 8".into());
                }
            }
        }
    }
    None
}

fn scalar_ty_is_none_and_not_unit(ty: &Ty) -> bool {
    !matches!(ty, Ty::Unit) && !is_scalar(ty)
}

fn is_scalar(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Bool | Ty::Char | Ty::Int(_) | Ty::IntLit | Ty::Float(_) | Ty::FloatLit
    )
}

/// Rvalues outside 5.2 scope (references, discriminants, aggregates).
fn rvalue_unsupported(rv: &Rvalue) -> Option<String> {
    match rv {
        Rvalue::Use(_) | Rvalue::Binary(..) | Rvalue::Unary(..) | Rvalue::Cast(..) => None,
        Rvalue::Ref(_) => Some("reference rvalue — Phase 6".into()),
        Rvalue::Discriminant(_) => Some("enum discriminant — Phase 5.4".into()),
        Rvalue::Aggregate(_, _) => Some("aggregate (tuple/struct/enum/closure) — Phase 5.4".into()),
    }
}

/// The `Const::Fn` symbols a function calls.
fn call_targets(f: &MirFn) -> Vec<String> {
    let mut out = Vec::new();
    for b in &f.blocks {
        if let Terminator::Call {
            func: Operand::Const(Const::Fn(name)),
            ..
        } = &b.term
        {
            out.push(name.clone());
        }
    }
    out
}

/// Per-function translation state.
struct FnGen<'a, 'ctx> {
    ctx: &'ctx Context,
    module: &'a Module<'ctx>,
    builder: Builder<'ctx>,
    decls: &'a HashMap<String, FunctionValue<'ctx>>,
    f: &'a MirFn,
    fval: FunctionValue<'ctx>,
    /// One `alloca` per local (`None` for unit locals, which hold no value).
    slots: Vec<Option<PointerValue<'ctx>>>,
    /// One LLVM block per MIR block.
    blocks: Vec<inkwell::basic_block::BasicBlock<'ctx>>,
}

impl<'a, 'ctx> FnGen<'a, 'ctx> {
    fn gen_fn(&mut self) -> Result<(), String> {
        // Entry block: stack slots for every local, then the params stored in.
        let entry = self.ctx.append_basic_block(self.fval, "entry");
        self.builder.position_at_end(entry);
        for l in &self.f.locals {
            let slot = match scalar_ty(self.ctx, &l.ty) {
                Some(t) => Some(self.b(self.builder.build_alloca(t, "slot"))?),
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
                let dst_ty = self.f.local_ty(place.local).clone();
                let v = self.gen_rvalue(rv, &dst_ty)?;
                if let (Some(slot), Some(v)) = (self.slots[place.local.0 as usize], v) {
                    self.b(self.builder.build_store(slot, v))?;
                }
            }
            // Drop / Storage* / Nop have no scalar codegen.
        }
        self.gen_term(&blk.term)
    }

    fn gen_term(&mut self, term: &Terminator) -> Result<(), String> {
        match term {
            Terminator::Return => {
                match self.slots[0] {
                    Some(slot) => {
                        let ty = scalar_ty(self.ctx, &self.f.locals[0].ty).unwrap();
                        let v = self.b(self.builder.build_load(ty, slot, "ret"))?;
                        self.b(self.builder.build_return(Some(&v)))?;
                    }
                    None => {
                        self.b(self.builder.build_return(None))?;
                    }
                }
            }
            Terminator::Goto(b) => {
                self.b(self.builder.build_unconditional_branch(self.blocks[b.0 as usize]))?;
            }
            Terminator::Unreachable => {
                self.b(self.builder.build_unreachable())?;
            }
            Terminator::Call { func, args, dest } => {
                let name = match func {
                    Operand::Const(Const::Fn(n)) => n,
                    _ => return Err("indirect call survived to codegen".into()),
                };
                let callee = *self
                    .decls
                    .get(name)
                    .ok_or_else(|| format!("call to undeclared fn `{name}`"))?;
                // Argument operands, each at its callee parameter type.
                let callee_mir_args = self.callee_arg_tys(name);
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    let expect = callee_mir_args.get(i).cloned().unwrap_or(Ty::Unknown);
                    let v = self
                        .gen_operand(a, &expect)?
                        .ok_or("unit-typed call argument")?;
                    argv.push(v.into());
                }
                let call = self
                    .b(self.builder.build_call(callee, &argv, "call"))?;
                if let Some((place, next)) = dest {
                    if let Some(slot) = self.slots[place.local.0 as usize] {
                        if let Some(v) = call.try_as_basic_value().basic() {
                            self.b(self.builder.build_store(slot, v))?;
                        }
                    }
                    self.b(self.builder.build_unconditional_branch(self.blocks[next.0 as usize]))?;
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
                self.b(self.builder.build_switch(
                    dv,
                    self.blocks[default.0 as usize],
                    &cases,
                ))?;
            }
        }
        Ok(())
    }

    /// Parameter types of the MIR callee with the given symbol (for typing
    /// constant arguments precisely).
    fn callee_arg_tys(&self, _sym: &str) -> Vec<Ty> {
        // We only need this for const-width precision; the declared LLVM param
        // types already pin widths, so an empty hint is safe (operands carry
        // their own type, and the only width-ambiguous operand — a float
        // constant — is rare as a direct call argument). Kept as a hook.
        Vec::new()
    }

    // -- rvalues / operands ------------------------------------------------

    fn gen_rvalue(&mut self, rv: &Rvalue, expected: &Ty) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        match rv {
            Rvalue::Use(op) => self.gen_operand(op, expected),
            Rvalue::Binary(op, a, b) => self.gen_binary(*op, a, b, expected).map(Some),
            Rvalue::Unary(op, a) => self.gen_unary(*op, a, expected).map(Some),
            Rvalue::Cast(op, ty) => self.gen_cast(op, ty).map(Some),
            _ => Err("non-scalar rvalue survived to codegen".into()),
        }
    }

    fn gen_operand(&mut self, op: &Operand, expected: &Ty) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                if !p.proj.is_empty() {
                    return Err("place projection survived to codegen".into());
                }
                match self.slots[p.local.0 as usize] {
                    Some(slot) => {
                        let ty = scalar_ty(self.ctx, self.f.local_ty(p.local)).unwrap();
                        Ok(Some(self.b(self.builder.build_load(ty, slot, "load"))?))
                    }
                    None => Ok(None), // unit local
                }
            }
            Operand::Const(c) => self.gen_const(c, expected),
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

    fn gen_binary(&mut self, op: BinOp, a: &Operand, b: &Operand, expected: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
        use BinOp::*;
        // Comparisons and logical connectives produce a bool from operands of
        // their own type; everything else produces a value of `expected`.
        match op {
            Eq | Ne | Lt | Gt | Le | Ge => {
                let ot = self.operand_ty(a).or_else(|| self.operand_ty(b)).unwrap_or_else(|| {
                    if self.is_float_const(a) || self.is_float_const(b) {
                        Ty::Float(FloatKind::F64)
                    } else {
                        Ty::Int(IntKind::I32)
                    }
                });
                let la = self.gen_operand(a, &ot)?.ok_or("unit operand in comparison")?;
                let lb = self.gen_operand(b, &ot)?.ok_or("unit operand in comparison")?;
                self.gen_compare(op, la, lb, &ot)
            }
            And | Or => {
                let la = self.gen_operand(a, &Ty::Bool)?.ok_or("unit operand in `&&`/`||`")?;
                let lb = self.gen_operand(b, &Ty::Bool)?.ok_or("unit operand in `&&`/`||`")?;
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
                let call = self.b(self.builder.build_call(powf, &[fa.into(), fb.into()], "pow"))?;
                Ok(call.try_as_basic_value().basic().ok_or("pow returned void")?)
            }
            _ => {
                // Arithmetic / bitwise, computed at `expected`.
                let la = self.gen_operand(a, expected)?.ok_or("unit operand in arithmetic")?;
                let lb = self.gen_operand(b, expected)?.ok_or("unit operand in arithmetic")?;
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
                            Div if signed => self.b(self.builder.build_int_signed_div(x, y, "sdiv"))?,
                            Div => self.b(self.builder.build_int_unsigned_div(x, y, "udiv"))?,
                            Rem if signed => self.b(self.builder.build_int_signed_rem(x, y, "srem"))?,
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

    fn gen_compare(&mut self, op: BinOp, la: BasicValueEnum<'ctx>, lb: BasicValueEnum<'ctx>, ot: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
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
                self.b(self.builder.build_float_compare(pred, la.into_float_value(), lb.into_float_value(), "fcmp"))?
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
                self.b(self.builder.build_int_compare(pred, la.into_int_value(), lb.into_int_value(), "icmp"))?
            }
        };
        Ok(r.into())
    }

    fn gen_unary(&mut self, op: UnOp, a: &Operand, expected: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self.gen_operand(a, expected)?.ok_or("unit operand in unary op")?;
        let r: BasicValueEnum = match op {
            UnOp::Neg => match expected {
                Ty::Float(_) | Ty::FloatLit => {
                    self.b(self.builder.build_float_neg(v.into_float_value(), "fneg"))?.into()
                }
                _ => self.b(self.builder.build_int_neg(v.into_int_value(), "neg"))?.into(),
            },
            // Logical not on a bool and bitwise complement on an int are both
            // LLVM `not` (xor with all-ones / with 1).
            UnOp::Not | UnOp::BitNot => self.b(self.builder.build_not(v.into_int_value(), "not"))?.into(),
            UnOp::Deref | UnOp::Ref | UnOp::RefMut | UnOp::RawRef => {
                return Err("ref/deref unary survived to codegen (Phase 6)".into());
            }
        };
        Ok(r)
    }

    fn gen_cast(&mut self, op: &Operand, target: &Ty) -> Result<BasicValueEnum<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Int(IntKind::I32));
        let v = self.gen_operand(op, &src_ty)?.ok_or("unit operand in cast")?;
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
                    self.b(self.builder.build_int_cast_sign_flag(v.into_int_value(), tt, src_signed, "icast"))?.into()
                } else {
                    // float → int truncates toward zero (matches the oracle).
                    let signed_target = match target {
                        Ty::Int(k) => is_signed(*k),
                        _ => false, // char
                    };
                    if signed_target {
                        self.b(self.builder.build_float_to_signed_int(v.into_float_value(), tt, "fptosi"))?.into()
                    } else {
                        self.b(self.builder.build_float_to_unsigned_int(v.into_float_value(), tt, "fptoui"))?.into()
                    }
                }
            }
            Ty::Float(_) | Ty::FloatLit => {
                let tt = scalar_ty(self.ctx, target).unwrap().into_float_type();
                if src_int {
                    if src_signed {
                        self.b(self.builder.build_signed_int_to_float(v.into_int_value(), tt, "sitofp"))?.into()
                    } else {
                        self.b(self.builder.build_unsigned_int_to_float(v.into_int_value(), tt, "uitofp"))?.into()
                    }
                } else {
                    self.b(self.builder.build_float_cast(v.into_float_value(), tt, "fpcast"))?.into()
                }
            }
            _ => return Err(format!("cast to {} is not a scalar (Phase 5.4+)", crate::ty::display_ty(target))),
        };
        Ok(r)
    }

    /// Convert an operand to `f64` (for `**`): int→float or float→f64.
    fn to_f64(&mut self, op: &Operand) -> Result<FloatValue<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Float(FloatKind::F64));
        let v = self.gen_operand(op, &src_ty)?.ok_or("unit operand in `**`")?;
        let f64t = self.ctx.f64_type();
        let r = match src_ty {
            Ty::Float(_) | Ty::FloatLit => self.b(self.builder.build_float_cast(v.into_float_value(), f64t, "topf"))?,
            Ty::Int(k) if is_signed(k) => self.b(self.builder.build_signed_int_to_float(v.into_int_value(), f64t, "topf"))?,
            _ => self.b(self.builder.build_unsigned_int_to_float(v.into_int_value(), f64t, "topf"))?,
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
            Operand::Copy(p) | Operand::Move(p) if p.proj.is_empty() => {
                Some(self.f.local_ty(p.local).clone())
            }
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

    // -- Phase 5.2: MIR → LLVM scalar/arithmetic translation, executed via JIT --
    //
    // The test inputs are deliberately within each type's range, so the
    // width-exact compiled result equals the interpreter oracle's i64/f64 result
    // (they only diverge at narrow-width overflow). Asserting the plainly-computed
    // value is therefore asserting oracle parity for these inputs.

    use inkwell::execution_engine::JitFunction;

    /// Lower a complete La3 source to MIR (front-end + HIR + mirgen).
    fn lower_to_mir(src: &str) -> crate::mir::MirProgram {
        let prog = crate::parser::parse(src).expect("parse");
        let errs = crate::checker::check(&prog);
        assert!(errs.is_empty(), "front-end errors: {errs:?}");
        let res = crate::checker::resolve(&prog);
        let table = crate::typeck::check_types(&prog);
        let hir = crate::hir::lower(&prog, &table, &res);
        crate::mirgen::lower(&hir).program
    }

    /// Build the LLVM module for `src` and return a JIT engine over it.
    fn jit<'ctx>(
        ctx: &'ctx Context,
        src: &str,
    ) -> inkwell::execution_engine::ExecutionEngine<'ctx> {
        let mir = lower_to_mir(src);
        let (module, _skipped) = build_program_module(ctx, &mir).expect("build module");
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
        let ee = jit(
            &ctx,
            "fn ud(a: u32, b: u32) -> u32 { a / b }\nfn main() {}",
        );
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
        let mir = lower_to_mir("fn s() -> str { \"hi\" }\nfn main() {}");
        let (_module, skipped) = build_program_module(&ctx, &mir).expect("build module");
        assert!(
            skipped.iter().any(|(sym, reason)| sym == "s" && reason.contains("non-scalar")),
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
            let c: JitFunction<unsafe extern "C" fn(i32) -> i32> =
                ee.get_function("classify").unwrap();
            assert_eq!(c.call(0), 100);
            assert_eq!(c.call(1), 200);
            assert_eq!(c.call(7), 999);
        }
    }
}

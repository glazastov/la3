//! Type mapping and small shared predicates: La3 `Ty` → LLVM types, the `str`
//! struct, the `math`/runtime-call predicates, and signedness. Split out of `codegen.rs`.

use super::*;

/// The LLVM type mirroring the runtime `La3Str` (`{ ptr, i64, i64 }`), used for
/// `str`-typed local storage. A `str`-returning runtime function (`from_utf8`,
/// `concat`, `fmt_*`) does **not** return this by value: LLVM does not implement
/// the C ABI for aggregate returns (it would mismatch Rust's sret `-> La3Str`),
/// so those are declared `void(ptr out, …)` and write through an explicit
/// out-pointer (which on x86_64 SysV is exactly the sret register, `rdi`).
pub(super) fn str_struct_ty(ctx: &Context) -> StructType<'_> {
    let ptr = ctx.ptr_type(AddressSpace::default());
    let i64t = ctx.i64_type();
    ctx.struct_type(&[ptr.into(), i64t.into(), i64t.into()], false)
}

pub(super) fn is_str(ty: &Ty) -> bool {
    matches!(ty, Ty::Str)
}

/// The numeric value of an inlined `math` constant (`math.pi`/`e`/`inf`), matching
/// the interpreter's `module_const` exactly.
pub(super) fn math_const(name: &str) -> Option<f64> {
    match name {
        "math.pi" => Some(std::f64::consts::PI),
        "math.e" => Some(std::f64::consts::E),
        "math.inf" => Some(f64::INFINITY),
        _ => None,
    }
}

/// Runtime/stdlib call symbols the back-end lowers directly to `la3_*` ABI calls
/// (Phase 6.1): f-string formatting / `str(x)` and the `io` writers.
pub(super) fn is_runtime_call(name: &str) -> bool {
    name == "std::format"
        || name == "str"
        || matches!(name, "io.print" | "io.println" | "io.eprintln")
}

/// The final symbol of a MIR function: `Owner::method` for a method, the bare
/// name for a free function — matching the `Const::Fn` symbol a call carries.
pub(super) fn fn_symbol(f: &MirFn) -> String {
    match &f.owner {
        Some(o) => format!("{}::{}", o, f.name),
        None => f.name.clone(),
    }
}

/// Whether an integer kind is signed (selects `sdiv`/`srem`/`ashr` and the
/// signed `icmp` predicates).
pub(super) fn is_signed(k: IntKind) -> bool {
    matches!(
        k,
        IntKind::I8 | IntKind::I16 | IntKind::I32 | IntKind::I64 | IntKind::Isize
    )
}

/// The LLVM function type for a MIR function. Aggregates are passed/returned as
/// `[size x i8]` byte storage (a self-consistent by-value ABI for la3↔la3
/// calls); a unit return is `void`. Only called for functions that passed
/// [`unsupported_reason`].
pub(super) fn fn_type<'ctx>(
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
pub(super) fn scalar_ty<'ctx>(ctx: &'ctx Context, ty: &Ty) -> Option<BasicTypeEnum<'ctx>> {
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
pub(super) fn storage_ty<'ctx>(
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
    // `str` is the runtime `La3Str` struct, held by value (Phase 6.1).
    if is_str(ty) {
        return Some(str_struct_ty(ctx).into());
    }
    // Aggregate: opaque byte storage of the by-value size.
    let (size, _) = oracle.size_align(ty)?;
    Some(ctx.i8_type().array_type(size as u32).into())
}

pub(super) fn int_ty<'ctx>(ctx: &'ctx Context, k: IntKind) -> IntType<'ctx> {
    match k {
        IntKind::I8 | IntKind::U8 => ctx.i8_type(),
        IntKind::I16 | IntKind::U16 => ctx.i16_type(),
        IntKind::I32 | IntKind::U32 => ctx.i32_type(),
        // isize/usize are 64-bit on the x86_64 v1 target.
        IntKind::I64 | IntKind::U64 | IntKind::Isize | IntKind::Usize => ctx.i64_type(),
    }
}

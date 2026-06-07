//! Supportedness analysis: which MIR functions the back-end can translate yet (the
//! skip-predicate) and the local-type inference it relies on. Split out of `codegen.rs`.

use super::*;

/// Reasons a function is out of current codegen scope (returns the first found).
/// Phase 5.4 adds **flat** aggregates (tuples/structs/enums whose fields are all
/// scalar) and their projections; nested aggregates, arrays, references, strings
/// and heap collections are still later phases.
pub(super) fn unsupported_reason(f: &MirFn, oracle: &LayoutOracle) -> Option<String> {
    let tys = infer_local_types(f, oracle);
    for (i, ty) in tys.iter().enumerate() {
        if let Some(r) = ty_unsupported(ty, oracle) {
            return Some(format!("local _{i}: {r}"));
        }
    }
    // Is this operand a `str` value? (str isn't an aggregate, so only bare
    // locals or string literals.)
    let op_is_str = |op: &Operand| match op {
        Operand::Copy(p) | Operand::Move(p) if p.proj.is_empty() => {
            is_str(&tys[p.local.0 as usize])
        }
        Operand::Const(Const::Str(_)) => true,
        _ => false,
    };
    for b in &f.blocks {
        for s in &b.stmts {
            if let Statement::Assign(p, rv) = s {
                if let Some(r) = proj_unsupported(&p.proj) {
                    return Some(r);
                }
                if let Some(r) = rvalue_unsupported(rv) {
                    return Some(r);
                }
                // `str` *comparison* (`==`/`<`/…, e.g. a `str` match arm) needs
                // `la3_str_eq` plus materializing literal operands — deferred to
                // Phase 6.2 (with collections); skip such functions cleanly.
                if let Rvalue::Binary(
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge,
                    a,
                    b,
                ) = rv
                {
                    if op_is_str(a) || op_is_str(b) {
                        return Some("str comparison — Phase 6.2".into());
                    }
                }
                // A global-value reference (a free fn used as a value, Phase 8)
                // as an operand: catch it here so the function is cleanly skipped
                // rather than hard-erroring deep in pass 3. (`str` literals and
                // `math` constants are supported, Phase 6.1.)
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
pub(super) fn rvalue_operands(rv: &Rvalue) -> Vec<&Operand> {
    match rv {
        Rvalue::Use(o) | Rvalue::Unary(_, o) | Rvalue::Cast(o, _) => vec![o],
        Rvalue::Binary(_, a, b) => vec![a, b],
        Rvalue::Aggregate(_, ops) => ops.iter().collect(),
        Rvalue::Ref(_) | Rvalue::Discriminant(_) => vec![],
    }
}

/// A global-value constant used as a *value* operand that the back-end cannot
/// lower — a free function used as a value (closure/fn-pointer ABI, Phase 8).
/// `str` literals and `math` constants are supported (Phase 6.1); a `Const::Fn`
/// in a call's `func` position is handled by the call lowering, not here.
pub(super) fn const_operand_unsupported(op: &Operand) -> Option<String> {
    match op {
        Operand::Const(Const::Fn(name)) if math_const(name).is_some() => None,
        Operand::Const(Const::Fn(name)) => {
            Some(format!("global-value reference `{name}` — Phase 8"))
        }
        _ => None,
    }
}

pub(super) fn is_scalar(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Bool | Ty::Char | Ty::Int(_) | Ty::IntLit | Ty::Float(_) | Ty::FloatLit
    )
}

/// `None` if `ty` is codegen-able here: a scalar, unit, or a **flat** aggregate
/// (tuple/struct/enum whose every field/payload is a scalar).
pub(super) fn ty_unsupported(ty: &Ty, oracle: &LayoutOracle) -> Option<String> {
    if is_scalar(ty) || matches!(ty, Ty::Unit) {
        return None;
    }
    // `str` is codegen-able (Phase 6.1), held by value as the runtime `La3Str`.
    if is_str(ty) {
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
pub(super) fn proj_unsupported(proj: &[Projection]) -> Option<String> {
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
pub(super) fn rvalue_unsupported(rv: &Rvalue) -> Option<String> {
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
pub(super) fn enum_ctor(name: &str, oracle: &LayoutOracle) -> Option<(String, usize)> {
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
pub(super) fn infer_local_types(f: &MirFn, oracle: &LayoutOracle) -> Vec<Ty> {
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
pub(super) fn call_targets(f: &MirFn, oracle: &LayoutOracle) -> Vec<String> {
    let mut out = Vec::new();
    for b in &f.blocks {
        if let Terminator::Call {
            func: Operand::Const(Const::Fn(name)),
            ..
        } = &b.term
        {
            if enum_ctor(name, oracle).is_none() && !is_runtime_call(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

//! Type relations and helpers, split out of `typeck.rs`: subtyping/assignability,
//! unification (`join`), generic binding collection and substitution, union
//! normalization, and small `Ty` utilities. `pub(super)` so the checker driver
//! in `typeck.rs` can call them.

use std::collections::{HashMap, HashSet};

use super::*;

impl Ty {
    pub(super) fn strip_future(self) -> Ty {
        match self {
            Ty::Future(inner) => *inner,
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Type relations
// ---------------------------------------------------------------------------

pub(super) fn arg0(args: &[Ty]) -> Ty {
    args.first().cloned().unwrap_or(Ty::Unknown)
}
pub(super) fn argn(args: &[Ty], n: usize) -> Ty {
    args.get(n).cloned().unwrap_or(Ty::Unknown)
}

pub(super) fn is_str_like(t: &Ty) -> bool {
    matches!(t, Ty::Str | Ty::Unknown) || matches!(t, Ty::Param(_))
}

pub(super) fn is_module(name: &str) -> bool {
    matches!(
        name,
        "io" | "fs" | "net" | "http" | "dns" | "tcp" | "bytes" | "crypto" | "json" | "os" | "math"
    )
}

pub(super) fn strip_ref(t: &Ty) -> Ty {
    match t {
        Ty::Ref(inner) => (**inner).clone(),
        other => other.clone(),
    }
}

pub(super) fn elem_ty(t: &Ty) -> Ty {
    match t {
        Ty::List(e) | Ty::Array(e, _) | Ty::Slice(e) | Ty::Set(e) | Ty::Range(e) => (**e).clone(),
        Ty::Map(k, v) => Ty::Tuple(vec![(**k).clone(), (**v).clone()]),
        Ty::Str => Ty::Char,
        Ty::Ref(inner) => elem_ty(inner),
        _ => Ty::Unknown,
    }
}

pub(super) fn concrete_int(t: &Ty) -> Ty {
    match t {
        Ty::IntLit => Ty::Int(IntKind::I32),
        Ty::Int(k) => Ty::Int(*k),
        _ => t.clone(),
    }
}

/// Pin any still-flexible literal to its default concrete type (reference
/// Section 2, rule 2: an unsuffixed integer literal defaults to `i32`, an
/// unsuffixed float literal to `f64`). Applied to the finished type table so the
/// back-end never sees an unresolved `{integer}`/`{float}`. Recurses through
/// compound types so e.g. `List<{integer}>` becomes `List<i32>`.
pub(super) fn default_ty(t: &Ty) -> Ty {
    use Ty::*;
    match t {
        IntLit => Int(IntKind::I32),
        FloatLit => Float(FloatKind::F64),
        Array(e, n) => Array(Box::new(default_ty(e)), *n),
        Slice(e) => Slice(Box::new(default_ty(e))),
        List(e) => List(Box::new(default_ty(e))),
        Set(e) => Set(Box::new(default_ty(e))),
        Range(e) => Range(Box::new(default_ty(e))),
        Map(k, v) => Map(Box::new(default_ty(k)), Box::new(default_ty(v))),
        Tuple(xs) => Tuple(xs.iter().map(default_ty).collect()),
        Struct(n, a) => Struct(n.clone(), a.iter().map(default_ty).collect()),
        Enum(n, a) => Enum(n.clone(), a.iter().map(default_ty).collect()),
        Union(ms) => Union(ms.iter().map(default_ty).collect()),
        Ref(x) => Ref(Box::new(default_ty(x))),
        Ptr(x) => Ptr(Box::new(default_ty(x))),
        Future(x) => Future(Box::new(default_ty(x))),
        Fn(ps, r) => Fn(ps.iter().map(default_ty).collect(), Box::new(default_ty(r))),
        other => other.clone(),
    }
}

pub(super) fn normalize_union(members: Vec<Ty>) -> Ty {
    let mut flat: Vec<Ty> = Vec::new();
    for m in members {
        match m {
            Ty::Union(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    flat.dedup_by(|a, b| a == b);
    match flat.len() {
        0 => Ty::Unknown,
        1 => flat.into_iter().next().unwrap(),
        _ => Ty::Union(flat),
    }
}

/// Numeric "share a type" rule for arithmetic (Section 4). `None` means the two
/// operands have different concrete types and an `as` cast is required.
pub(super) fn num_join(a: &Ty, b: &Ty) -> Option<Ty> {
    use Ty::*;
    match (a, b) {
        (Unknown, x) | (x, Unknown) => {
            if x.is_numeric() {
                Some(x.clone())
            } else {
                Some(Unknown)
            }
        }
        (Param(_), x) | (x, Param(_)) => Some(x.clone()),
        (IntLit, IntLit) => Some(IntLit),
        (IntLit, Int(k)) | (Int(k), IntLit) => Some(Int(*k)),
        (Int(k1), Int(k2)) if k1 == k2 => Some(Int(*k1)),
        (FloatLit, FloatLit) => Some(FloatLit),
        (FloatLit, Float(k)) | (Float(k), FloatLit) => Some(Float(*k)),
        (Float(k1), Float(k2)) if k1 == k2 => Some(Float(*k1)),
        _ => None,
    }
}

/// Least upper bound used to merge branch/element types. Falls back to the
/// concrete side when one operand is a flexible literal or `Unknown`.
pub(super) fn join(a: &Ty, b: &Ty) -> Ty {
    use Ty::*;
    match (a, b) {
        (Unknown, x) | (x, Unknown) => x.clone(),
        (Never, x) | (x, Never) => x.clone(),
        _ if a == b => a.clone(),
        (IntLit, Int(k)) | (Int(k), IntLit) => Int(*k),
        (FloatLit, Float(k)) | (Float(k), FloatLit) => Float(*k),
        (Nil, x) | (x, Nil) => {
            if x.is_optional() {
                x.clone()
            } else {
                Ty::option(x.clone())
            }
        }
        // Merge an `Enum` payload (covers Option/Result branch joins).
        (Enum(n1, a1), Enum(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            let merged: Vec<Ty> = a1.iter().zip(a2.iter()).map(|(x, y)| join(x, y)).collect();
            Enum(n1.clone(), merged)
        }
        (List(x), List(y)) => List(Box::new(join(x, y))),
        _ => a.clone(),
    }
}

/// Are two types compatible enough to appear in the same branch position? This
/// is symmetric and lenient (literals, `Unknown`, `Param`, and `Never` all fit).
pub(super) fn unifies(a: &Ty, b: &Ty) -> bool {
    assignable(a, b) || assignable(b, a)
}

/// Is a value of type `from` usable where `to` is expected?
pub(super) fn assignable(from: &Ty, to: &Ty) -> bool {
    use Ty::*;
    if from == to {
        return true;
    }
    match (from, to) {
        (Unknown, _) | (_, Unknown) => true,
        (Never, _) => true,
        (Param(_), _) | (_, Param(_)) => true,
        (IntLit, Int(_)) | (Int(_), IntLit) | (IntLit, IntLit) => true,
        (FloatLit, Float(_)) | (Float(_), FloatLit) | (FloatLit, FloatLit) => true,
        // nil is the absent case of any optional.
        (Nil, t) | (t, Nil) => t.is_optional() || matches!(t, Nil),
        // Bare optional `T | nil` and `Option<T>` are the same value (Section 2).
        (Enum(n, args), other) | (other, Enum(n, args)) if n == "Option" => {
            let inner = args.first().cloned().unwrap_or(Unknown);
            match other {
                Enum(n2, a2) if n2 == "Option" => {
                    assignable(&inner, &a2.first().cloned().unwrap_or(Unknown))
                }
                Union(ms) => ms
                    .iter()
                    .all(|m| matches!(m, Nil) || assignable(m, &inner) || assignable(&inner, m)),
                Nil => true,
                _ => assignable(other, &inner) || assignable(&inner, other),
            }
        }
        (Enum(n1, a1), Enum(n2, a2)) if n1 == n2 => {
            a1.len() == a2.len() && a1.iter().zip(a2).all(|(x, y)| assignable(x, y))
        }
        (Struct(n1, a1), Struct(n2, a2)) if n1 == n2 => {
            a1.len() == a2.len() && a1.iter().zip(a2).all(|(x, y)| assignable(x, y))
        }
        // Sequence forms interconvert leniently (a list literal coerces to an
        // array or slice from context, Section 2).
        (List(x), List(y))
        | (List(x), Slice(y))
        | (Slice(x), List(y))
        | (Slice(x), Slice(y))
        | (Array(x, _), Slice(y))
        | (Array(x, _), List(y))
        | (List(x), Array(y, _))
        | (Set(x), Set(y))
        | (Range(x), Range(y)) => assignable(x, y),
        (Array(x, n1), Array(y, n2)) => {
            (n1 == n2 || n1.is_none() || n2.is_none()) && assignable(x, y)
        }
        (Map(k1, v1), Map(k2, v2)) => assignable(k1, k2) && assignable(v1, v2),
        (Tuple(a), Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| assignable(x, y))
        }
        (Ref(x), Ref(y)) | (Ptr(x), Ptr(y)) | (Future(x), Future(y)) => assignable(x, y),
        (Ref(x), y) => assignable(x, y),
        (x, Ref(y)) => assignable(x, y),
        (Union(ms), to) => ms.iter().all(|m| assignable(m, to)),
        (from, Union(ms)) => ms.iter().any(|m| assignable(from, m)),
        (Fn(p1, r1), Fn(p2, r2)) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2).all(|(x, y)| assignable(x, y))
                && assignable(r1, r2)
        }
        _ => false,
    }
}

/// Match a parameter type that mentions generic params against a concrete
/// argument type, recording the inferred bindings.
pub(super) fn collect_param_bindings(param: &Ty, arg: &Ty, out: &mut HashMap<String, Ty>) {
    match (param, arg) {
        (Ty::Param(n), a) => {
            out.entry(n.clone()).or_insert_with(|| a.clone());
        }
        (Ty::List(p), Ty::List(a))
        | (Ty::Slice(p), Ty::Slice(a))
        | (Ty::Slice(p), Ty::List(a))
        | (Ty::List(p), Ty::Slice(a))
        | (Ty::Set(p), Ty::Set(a))
        | (Ty::Ref(p), Ty::Ref(a))
        | (Ty::Future(p), Ty::Future(a)) => collect_param_bindings(p, a, out),
        (Ty::Ref(p), a) => collect_param_bindings(p, a, out),
        (p, Ty::Ref(a)) => collect_param_bindings(p, a, out),
        (Ty::Map(pk, pv), Ty::Map(ak, av)) => {
            collect_param_bindings(pk, ak, out);
            collect_param_bindings(pv, av, out);
        }
        (Ty::Tuple(ps), Ty::Tuple(as_)) if ps.len() == as_.len() => {
            for (p, a) in ps.iter().zip(as_) {
                collect_param_bindings(p, a, out);
            }
        }
        (Ty::Enum(n1, ps), Ty::Enum(n2, as_)) if n1 == n2 => {
            for (p, a) in ps.iter().zip(as_) {
                collect_param_bindings(p, a, out);
            }
        }
        (Ty::Struct(n1, ps), Ty::Struct(n2, as_)) if n1 == n2 => {
            for (p, a) in ps.iter().zip(as_) {
                collect_param_bindings(p, a, out);
            }
        }
        _ => {}
    }
}

/// Substitute resolved generic bindings into a type.
pub(super) fn subst(t: &Ty, bindings: &HashMap<String, Ty>) -> Ty {
    match t {
        Ty::Param(n) => bindings.get(n).cloned().unwrap_or_else(|| t.clone()),
        Ty::List(e) => Ty::List(Box::new(subst(e, bindings))),
        Ty::Slice(e) => Ty::Slice(Box::new(subst(e, bindings))),
        Ty::Set(e) => Ty::Set(Box::new(subst(e, bindings))),
        Ty::Array(e, n) => Ty::Array(Box::new(subst(e, bindings)), *n),
        Ty::Range(e) => Ty::Range(Box::new(subst(e, bindings))),
        Ty::Ref(e) => Ty::Ref(Box::new(subst(e, bindings))),
        Ty::Ptr(e) => Ty::Ptr(Box::new(subst(e, bindings))),
        Ty::Future(e) => Ty::Future(Box::new(subst(e, bindings))),
        Ty::Map(k, v) => Ty::Map(Box::new(subst(k, bindings)), Box::new(subst(v, bindings))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst(t, bindings)).collect()),
        Ty::Union(ts) => normalize_union(ts.iter().map(|t| subst(t, bindings)).collect()),
        Ty::Enum(n, args) => Ty::Enum(n.clone(), args.iter().map(|t| subst(t, bindings)).collect()),
        Ty::Struct(n, args) => {
            Ty::Struct(n.clone(), args.iter().map(|t| subst(t, bindings)).collect())
        }
        Ty::Fn(ps, r) => Ty::Fn(
            ps.iter().map(|t| subst(t, bindings)).collect(),
            Box::new(subst(r, bindings)),
        ),
        _ => t.clone(),
    }
}

pub(super) fn wrap_optional(t: Ty, optional: bool) -> Ty {
    if optional && !t.is_optional() {
        Ty::option(t)
    } else {
        t
    }
}

/// If `e` is a borrow of a root variable (`&x`, `&mut x`, `&raw x`, `&arr[i]`),
/// return `(variable name, is the borrow mutable)`.
pub(super) fn borrow_root(e: &Expr) -> Option<(String, bool)> {
    let (op, inner) = match &e.kind {
        ExprKind::Unary { op, expr } => (*op, expr),
        _ => return None,
    };
    let mutable = matches!(op, UnOp::RefMut | UnOp::RawRef);
    if !matches!(op, UnOp::Ref | UnOp::RefMut | UnOp::RawRef) {
        return None;
    }
    // Peel index/field access down to the root identifier.
    let mut cur = inner.as_ref();
    loop {
        match &cur.kind {
            ExprKind::Ident(name) => return Some((name.clone(), mutable)),
            ExprKind::Index { recv, .. } | ExprKind::Field { recv, .. } => cur = recv,
            _ => return None,
        }
    }
}

pub(super) fn collect_covered_variants(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Variant { path, .. } => {
            if let Some(last) = path.last() {
                out.insert(last.clone());
            }
        }
        // A struct-variant pattern `Enum.Variant { .. }` keeps the dotted name.
        Pattern::Struct { name, .. } => {
            let variant = name.rsplit('.').next().unwrap_or(name);
            out.insert(variant.to_string());
        }
        Pattern::Nil => {
            out.insert("None".into());
        }
        Pattern::Or(ps) => {
            for p in ps {
                collect_covered_variants(p, out);
            }
        }
        Pattern::At(_, sub) => collect_covered_variants(sub, out),
        _ => {}
    }
}

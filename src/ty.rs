//! The semantic type `Ty` — the compiler's internal notion of a type, distinct
//! from [`crate::ast::TypeExpr`] (the surface syntax).
//!
//! Extracted from `typeck` so it can be **shared**: the type checker produces it,
//! the borrow checker queries it (via `TypeTable`), and the HIR/back-end embed it
//! in their nodes (Phase 2 onward) without re-inferring. Kept `pub(crate)` — it is
//! an internal compiler representation, not a public API.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IntKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Isize,
    Usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FloatKind {
    F32,
    F64,
}

/// A semantic type. Distinct from [`crate::ast::TypeExpr`], which is the surface
/// syntax.
///
/// `Eq`/`Hash` are derived (there is no `f64` payload) so the MIR can key
/// monomorphized instances by concrete type. `IntLit`/`FloatLit` are inference
/// artifacts only — after the `default_ty` pass (Phase 1.5) a recorded type is
/// concrete, so the HIR/back-end should never observe them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ty {
    Bool,
    Int(IntKind),
    /// An unsuffixed integer literal, not yet pinned to a width (defaults to
    /// `i32`). Compatible with any concrete integer type.
    IntLit,
    Float(FloatKind),
    /// An unsuffixed float literal (defaults to `f64`).
    FloatLit,
    Char,
    /// An **owned**, immutable UTF-8 string (heap-backed, move-only, dropped) —
    /// not a borrowed view. The borrowed counterpart is `Slice` (`&[u8]`).
    Str,
    Nil,
    Unit,
    Never,
    Array(Box<Ty>, Option<usize>),
    Slice(Box<Ty>),
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Set(Box<Ty>),
    Tuple(Vec<Ty>),
    Range(Box<Ty>),
    /// A nominal struct or enum, with resolved generic arguments. `Option<T>`
    /// and `Result<T>` are `Enum("Option", _)` / `Enum("Result", _)`.
    Struct(String, Vec<Ty>),
    Enum(String, Vec<Ty>),
    Fn(Vec<Ty>, Box<Ty>),
    Union(Vec<Ty>),
    Ref(Box<Ty>),
    Ptr(Box<Ty>),
    Future(Box<Ty>),
    /// A generic type parameter in scope (e.g. `T`).
    Param(String),
    /// Type could not be determined; compatible with everything.
    Unknown,
}

impl Ty {
    pub(crate) fn option(inner: Ty) -> Ty {
        Ty::Enum("Option".into(), vec![inner])
    }
    pub(crate) fn result(inner: Ty) -> Ty {
        Ty::Enum("Result".into(), vec![inner])
    }
    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
    }
    pub(crate) fn is_int(&self) -> bool {
        matches!(self, Ty::Int(_) | Ty::IntLit)
    }
    pub(crate) fn is_float(&self) -> bool {
        matches!(self, Ty::Float(_) | Ty::FloatLit)
    }
    pub(crate) fn is_numeric(&self) -> bool {
        self.is_int() || self.is_float()
    }
    /// Does this type include `nil` (a bare optional or an `Option`)?
    pub(crate) fn is_optional(&self) -> bool {
        match self {
            Ty::Nil | Ty::Unknown => true,
            Ty::Enum(n, _) if n == "Option" => true,
            Ty::Union(ms) => ms.iter().any(|m| matches!(m, Ty::Nil)),
            _ => false,
        }
    }
    /// The non-`nil` payload of an optional, used by `??` and `?.`.
    pub(crate) fn strip_nil(&self) -> Ty {
        match self {
            Ty::Enum(n, args) if n == "Option" => args.first().cloned().unwrap_or(Ty::Unknown),
            Ty::Union(ms) => {
                let rest: Vec<Ty> = ms
                    .iter()
                    .filter(|m| !matches!(m, Ty::Nil))
                    .cloned()
                    .collect();
                match rest.len() {
                    0 => Ty::Unknown,
                    1 => rest.into_iter().next().unwrap(),
                    _ => Ty::Union(rest),
                }
            }
            Ty::Nil => Ty::Unknown,
            other => other.clone(),
        }
    }
}

pub(crate) fn int_kind(name: &str) -> Option<IntKind> {
    Some(match name {
        "i8" => IntKind::I8,
        "i16" => IntKind::I16,
        "i32" => IntKind::I32,
        "i64" => IntKind::I64,
        "u8" | "byte" => IntKind::U8,
        "u16" => IntKind::U16,
        "u32" => IntKind::U32,
        "u64" => IntKind::U64,
        "isize" => IntKind::Isize,
        "usize" => IntKind::Usize,
        _ => return None,
    })
}

/// Render a type the way it is written in source (`i32`, `List<str>`, `&T`, …),
/// used by `la3 types`/`la3 layout` and diagnostics.
pub(crate) fn display_ty(t: &Ty) -> String {
    match t {
        Ty::Bool => "bool".into(),
        Ty::Int(k) => format!("{:?}", k).to_lowercase(),
        Ty::IntLit => "{integer}".into(),
        Ty::Float(k) => format!("{:?}", k).to_lowercase(),
        Ty::FloatLit => "{float}".into(),
        Ty::Char => "char".into(),
        Ty::Str => "str".into(),
        Ty::Nil => "nil".into(),
        Ty::Unit => "()".into(),
        Ty::Never => "!".into(),
        Ty::Array(e, n) => match n {
            Some(n) => format!("[{}; {}]", display_ty(e), n),
            None => format!("[{}]", display_ty(e)),
        },
        Ty::Slice(e) => format!("&[{}]", display_ty(e)),
        Ty::List(e) => format!("List<{}>", display_ty(e)),
        Ty::Map(k, v) => format!("Map<{}, {}>", display_ty(k), display_ty(v)),
        Ty::Set(e) => format!("Set<{}>", display_ty(e)),
        Ty::Tuple(ts) => {
            let inner: Vec<String> = ts.iter().map(display_ty).collect();
            format!("({})", inner.join(", "))
        }
        Ty::Range(e) => format!("Range<{}>", display_ty(e)),
        Ty::Struct(n, args) | Ty::Enum(n, args) => {
            if args.is_empty() {
                n.clone()
            } else {
                let inner: Vec<String> = args.iter().map(display_ty).collect();
                format!("{}<{}>", n, inner.join(", "))
            }
        }
        Ty::Fn(ps, r) => {
            let inner: Vec<String> = ps.iter().map(display_ty).collect();
            format!("fn({}) -> {}", inner.join(", "), display_ty(r))
        }
        Ty::Union(ms) => {
            let inner: Vec<String> = ms.iter().map(display_ty).collect();
            inner.join(" | ")
        }
        Ty::Ref(t) => format!("&{}", display_ty(t)),
        Ty::Ptr(t) => format!("*{}", display_ty(t)),
        Ty::Future(t) => format!("async {}", display_ty(t)),
        Ty::Param(n) => n.clone(),
        Ty::Unknown => "_".into(),
    }
}

/// Is a value of type `t` implicitly copyable (Section 9 `Copy`)? Scalars,
/// `nil`, references, raw pointers, slices, ranges, and `fn` are Copy; owned
/// heap data (`str`, `List`/`Map`/`Set`, structs, enums, futures, unions) is
/// move-only. Aggregates are Copy exactly when every element is. `Unknown` and
/// generic `Param` are treated as Copy so the borrow checker stays lenient on
/// types it cannot fully model.
pub(crate) fn ty_is_copy(t: &Ty) -> bool {
    use Ty::*;
    match t {
        Bool
        | Int(_)
        | IntLit
        | Float(_)
        | FloatLit
        | Char
        | Unit
        | Never
        | Nil
        | Ref(_)
        | Ptr(_)
        | Slice(_)
        | Range(_)
        | Fn(_, _)
        | Unknown
        | Param(_) => true,
        Tuple(xs) => xs.iter().all(ty_is_copy),
        Array(e, _) => ty_is_copy(e),
        Str | List(_) | Map(_, _) | Set(_) | Struct(_, _) | Enum(_, _) | Future(_) | Union(_) => {
            false
        }
    }
}

//! The La3 type system (reference Sections 2, 4, 7, 9).
//!
//! This pass runs after name resolution ([`crate::checker`]) and enforces the
//! typing rules the language specification states explicitly:
//!
//! * **Section 2 (Types).** Type inference for `let`/`const`, the `i32`/`f64`
//!   literal defaults, no implicit numeric widening or narrowing (an `as` cast
//!   is required for every conversion), the `nil` / `Option<T>` identity, and
//!   union narrowing through `match`.
//! * **Section 4 (Operators).** Operand typing for arithmetic (operands must
//!   share a type), `**` always yields `f64`, comparison and logical operators
//!   always yield `bool`, bitwise operators require integers, and `??` / `?.`
//!   operate on a bare optional `T | nil`.
//! * **Section 7 (Control flow).** `if` and `match` are expressions whose arms
//!   must agree on a type, and `match` is exhaustive.
//! * **Section 9 (Interfaces).** Conformance is nominal: a generic bound
//!   `T: Iface` is satisfied only when an explicit `impl Iface for T` exists.
//!
//! Inference is deliberately *sound but lenient*: when a type cannot be
//! determined it becomes [`Ty::Unknown`], which is compatible with everything,
//! so the checker reports genuine mistakes without inventing false positives for
//! the parts of the standard library it does not model in full.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos};

mod builtins;
mod calls;
mod collect;
mod control;
mod driver;
mod infer;
mod layout;
mod relations;
mod stmts;
use builtins::*;
pub use layout::*;
use relations::*;

/// The type checker's full product: the diagnostics plus a concrete type for
/// every expression node, keyed by [`NodeId`]. The compiler back-end consumes
/// the table; `la3 types` dumps it. The program must already be numbered
/// (`Program::assign_ids`, done by `parser::parse`).
pub struct TypeTable {
    map: HashMap<NodeId, Ty>,
    order: Vec<(Pos, NodeId)>,
    pub errors: Vec<Diagnostic>,
}

impl TypeTable {
    /// The inferred type of a node, rendered as written in source (e.g. `i32`,
    /// `List<str>`, `Option<i32>`). `None` if the node was never typed.
    pub fn type_of(&self, id: NodeId) -> Option<String> {
        self.map.get(&id).map(display_ty)
    }

    /// Is the value of node `id` implicitly copyable (so reusing the binding
    /// after a by-value use is fine)? Consumed by the borrow checker
    /// ([`crate::borrowck`]). A node with no recorded type is treated as Copy so
    /// the checker never invents a move on a type it does not model.
    pub fn is_copy(&self, id: NodeId) -> bool {
        self.map.get(&id).map_or(true, ty_is_copy)
    }

    /// Number of typed expression nodes. Used by the back-end (Phase 4+).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// One `line:col  <type>` line per expression, in source order, for the
    /// `la3 types` debugging command.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (pos, id) in &self.order {
            if let Some(t) = self.type_of(*id) {
                out.push_str(&format!("{:>4}:{:<3} {}\n", pos.line, pos.col, t));
            }
        }
        out
    }
}

/// Run the type checker and return the full [`TypeTable`] (types + errors).
pub fn check_types(prog: &Program) -> TypeTable {
    let mut tc = TypeChecker::new(prog);
    tc.run(prog);
    // Pin any literal left flexible by inference to its default (i32/f64), so the
    // recorded table the back-end consumes is fully concrete (Section 2, rule 2).
    let map = tc
        .types
        .into_iter()
        .map(|(id, t)| (id, default_ty(&t)))
        .collect();
    TypeTable {
        map,
        order: tc.type_order,
        errors: tc.errors,
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntKind {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatKind {
    F32,
    F64,
}

/// A semantic type. Distinct from [`TypeExpr`], which is the surface syntax.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Bool,
    Int(IntKind),
    /// An unsuffixed integer literal, not yet pinned to a width (defaults to
    /// `i32`). Compatible with any concrete integer type.
    IntLit,
    Float(FloatKind),
    /// An unsuffixed float literal (defaults to `f64`).
    FloatLit,
    Char,
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
    fn option(inner: Ty) -> Ty {
        Ty::Enum("Option".into(), vec![inner])
    }
    fn result(inner: Ty) -> Ty {
        Ty::Enum("Result".into(), vec![inner])
    }
    fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
    }
    fn is_int(&self) -> bool {
        matches!(self, Ty::Int(_) | Ty::IntLit)
    }
    fn is_float(&self) -> bool {
        matches!(self, Ty::Float(_) | Ty::FloatLit)
    }
    fn is_numeric(&self) -> bool {
        self.is_int() || self.is_float()
    }
    /// Does this type include `nil` (a bare optional or an `Option`)?
    fn is_optional(&self) -> bool {
        match self {
            Ty::Nil | Ty::Unknown => true,
            Ty::Enum(n, _) if n == "Option" => true,
            Ty::Union(ms) => ms.iter().any(|m| matches!(m, Ty::Nil)),
            _ => false,
        }
    }
    /// The non-`nil` payload of an optional, used by `??` and `?.`.
    fn strip_nil(&self) -> Ty {
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

fn int_kind(name: &str) -> Option<IntKind> {
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

fn display_ty(t: &Ty) -> String {
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
fn ty_is_copy(t: &Ty) -> bool {
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

// ---------------------------------------------------------------------------
// Declaration tables
// ---------------------------------------------------------------------------

struct StructInfo {
    generics: Vec<String>,
    fields: Vec<(String, TypeExpr)>,
}

struct EnumInfo {
    generics: Vec<String>,
    variants: Vec<EnumVariant>,
}

#[derive(Clone)]
struct FnSig {
    generics: Vec<(String, Vec<String>)>, // (name, interface bounds)
    params: Vec<Ty>,
    ret: Ty,
}

struct MethodSig {
    sig: FnSig,
}

struct InterfaceInfo {
    supers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Checker
// ---------------------------------------------------------------------------

struct TypeChecker {
    structs: HashMap<String, StructInfo>,
    enums: HashMap<String, EnumInfo>,
    fns: HashMap<String, FnSig>,
    consts: HashMap<String, Ty>,
    aliases: HashMap<String, TypeExpr>,
    interfaces: HashMap<String, InterfaceInfo>,
    /// Methods keyed by `(type_name, method_name)`.
    methods: HashMap<(String, String), MethodSig>,
    /// Explicit conformances `(interface, type_name)` from `impl I for T`.
    impls: HashSet<(String, String)>,

    scopes: Vec<HashMap<String, Ty>>,
    /// Generic parameter names visible in the current item.
    type_params: HashSet<String>,
    /// Return type of the function currently being checked.
    ret_stack: Vec<Ty>,
    /// Nesting depth of `unsafe` blocks; raw-pointer dereference is only allowed
    /// while this is greater than zero (reference Section 11).
    unsafe_depth: u32,
    /// `break` value types for each enclosing `loop`.
    loop_breaks: Vec<Vec<Ty>>,

    /// Inferred type of every expression, keyed by [`NodeId`]. This is the
    /// product the compiler back-end consumes; the interpreter ignores it.
    types: HashMap<NodeId, Ty>,
    /// `(pos, id)` in the order expressions were inferred, so a dump can print
    /// the table in roughly source order without re-walking the tree.
    type_order: Vec<(Pos, NodeId)>,

    errors: Vec<Diagnostic>,
}

/// Interfaces every primitive is assumed to satisfy, so a bound like
/// `T: Ord` never demands an `impl` for `i32`.
fn builtin_interface(name: &str) -> bool {
    matches!(
        name,
        "Ord" | "Eq" | "PartialEq" | "Clone" | "Copy" | "Hash" | "Display" | "Debug"
    )
}

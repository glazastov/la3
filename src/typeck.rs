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
use crate::ty::*;

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

    /// Number of typed expression nodes. Used by the back-end (HIR/MIR onward).
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

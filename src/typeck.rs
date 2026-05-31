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

pub fn check(prog: &Program) -> Vec<Diagnostic> {
    let mut tc = TypeChecker::new(prog);
    tc.run(prog);
    tc.errors
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

impl TypeChecker {
    fn new(prog: &Program) -> Self {
        let mut tc = TypeChecker {
            structs: HashMap::new(),
            enums: HashMap::new(),
            fns: HashMap::new(),
            consts: HashMap::new(),
            aliases: HashMap::new(),
            interfaces: HashMap::new(),
            methods: HashMap::new(),
            impls: HashSet::new(),
            scopes: Vec::new(),
            type_params: HashSet::new(),
            ret_stack: Vec::new(),
            loop_breaks: Vec::new(),
            unsafe_depth: 0,
            errors: Vec::new(),
        };
        tc.collect(prog);
        tc
    }

    /// First pass: record every top-level declaration so forward references and
    /// mutual recursion resolve.
    fn collect(&mut self, prog: &Program) {
        // Type-shaped declarations first, so signatures can resolve them.
        for item in &prog.items {
            match item {
                Item::Struct(s) => {
                    self.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            generics: s.generics.clone(),
                            fields: s.fields.clone(),
                        },
                    );
                }
                Item::Enum(e) => {
                    self.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            generics: e.generics.clone(),
                            variants: e.variants.clone(),
                        },
                    );
                }
                Item::TypeAlias { name, ty } => {
                    self.aliases.insert(name.clone(), ty.clone());
                }
                Item::Interface(i) => {
                    self.interfaces.insert(
                        i.name.clone(),
                        InterfaceInfo {
                            supers: i.supers.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        // Now functions, consts, and impl blocks, whose signatures reference types.
        for item in &prog.items {
            match item {
                Item::Fn(f) => {
                    let sig = self.fn_sig(f, &HashSet::new());
                    self.fns.insert(f.name.clone(), sig);
                }
                Item::Const(c) => {
                    let ty =
                        c.ty.as_ref()
                            .map(|t| self.resolve(t))
                            .unwrap_or(Ty::Unknown);
                    self.consts.insert(c.name.clone(), ty);
                }
                Item::Impl(b) => {
                    let owner_generics: HashSet<String> = self
                        .structs
                        .get(&b.ty)
                        .map(|s| s.generics.clone())
                        .or_else(|| self.enums.get(&b.ty).map(|e| e.generics.clone()))
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    if let Some(iface) = &b.interface {
                        self.impls.insert((iface.clone(), b.ty.clone()));
                    }
                    for m in &b.methods {
                        let sig = self.fn_sig(m, &owner_generics);
                        self.methods
                            .insert((b.ty.clone(), m.name.clone()), MethodSig { sig });
                    }
                }
                _ => {}
            }
        }
    }

    fn fn_sig(&self, f: &FnDecl, extra_generics: &HashSet<String>) -> FnSig {
        let mut generics_set: HashSet<String> = extra_generics.clone();
        for (g, _) in &f.generics {
            generics_set.insert(g.clone());
        }
        let params = f
            .params
            .iter()
            .filter(|p| !p.is_self)
            .map(|p| {
                p.ty.as_ref()
                    .map(|t| self.resolve_in(t, &generics_set))
                    .unwrap_or(Ty::Unknown)
            })
            .collect();
        let mut ret = f
            .ret
            .as_ref()
            .map(|t| self.resolve_in(t, &generics_set))
            .unwrap_or(Ty::Unit);
        if f.is_async {
            ret = Ty::Future(Box::new(ret));
        }
        let generics = f.generics.clone();
        FnSig {
            generics,
            params,
            ret,
        }
    }

    // ---- type resolution -------------------------------------------------

    fn resolve(&self, t: &TypeExpr) -> Ty {
        let params = self.type_params.clone();
        self.resolve_in(t, &params)
    }

    fn resolve_in(&self, t: &TypeExpr, generics: &HashSet<String>) -> Ty {
        match t {
            TypeExpr::Named { name, args } => {
                let rargs: Vec<Ty> = args.iter().map(|a| self.resolve_in(a, generics)).collect();
                if generics.contains(name) && rargs.is_empty() {
                    return Ty::Param(name.clone());
                }
                match name.as_str() {
                    "bool" => Ty::Bool,
                    "char" => Ty::Char,
                    "str" => Ty::Str,
                    "nil" => Ty::Nil,
                    "f32" => Ty::Float(FloatKind::F32),
                    "f64" => Ty::Float(FloatKind::F64),
                    "List" | "Vec" => Ty::List(Box::new(arg0(&rargs))),
                    "Set" => Ty::Set(Box::new(arg0(&rargs))),
                    "Map" => Ty::Map(Box::new(arg0(&rargs)), Box::new(argn(&rargs, 1))),
                    "Option" => Ty::option(arg0(&rargs)),
                    "Result" => Ty::result(arg0(&rargs)),
                    "any" => Ty::Unknown,
                    _ => {
                        if let Some(k) = int_kind(name) {
                            Ty::Int(k)
                        } else if let Some(alias) = self.aliases.get(name) {
                            self.resolve_in(&alias.clone(), generics)
                        } else if self.structs.contains_key(name) {
                            Ty::Struct(name.clone(), rargs)
                        } else if self.enums.contains_key(name) {
                            Ty::Enum(name.clone(), rargs)
                        } else {
                            // Interfaces used as a type, or anything unknown.
                            Ty::Unknown
                        }
                    }
                }
            }
            TypeExpr::Ref { inner, .. } => Ty::Ref(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Ptr { inner, .. } => Ty::Ptr(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Array { inner, size } => Ty::Array(
                Box::new(self.resolve_in(inner, generics)),
                size.map(|s| s as usize),
            ),
            TypeExpr::Slice(inner) => Ty::Slice(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Tuple(ts) => {
                Ty::Tuple(ts.iter().map(|t| self.resolve_in(t, generics)).collect())
            }
            TypeExpr::Union(ts) => {
                let members: Vec<Ty> = ts.iter().map(|t| self.resolve_in(t, generics)).collect();
                normalize_union(members)
            }
            TypeExpr::Fn { params, ret } => Ty::Fn(
                params
                    .iter()
                    .map(|t| self.resolve_in(t, generics))
                    .collect(),
                Box::new(self.resolve_in(ret, generics)),
            ),
            TypeExpr::Async(inner) => Ty::Future(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Unit => Ty::Unit,
            TypeExpr::Never => Ty::Never,
        }
    }

    // ---- driver ----------------------------------------------------------

    fn run(&mut self, prog: &Program) {
        for item in &prog.items {
            match item {
                Item::Fn(f) => self.check_fn(f, &HashSet::new()),
                Item::Impl(b) => {
                    let owner_generics: HashSet<String> = self
                        .structs
                        .get(&b.ty)
                        .map(|s| s.generics.clone())
                        .or_else(|| self.enums.get(&b.ty).map(|e| e.generics.clone()))
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    let self_ty = self.named_self_ty(&b.ty);
                    for m in &b.methods {
                        self.check_fn_with_self(m, &owner_generics, Some(self_ty.clone()));
                    }
                    if let Some(iface) = &b.interface {
                        self.check_conformance(b, iface);
                    }
                }
                Item::Const(c) => {
                    let declared = c.ty.as_ref().map(|t| self.resolve(t));
                    let got = self.infer(&c.value);
                    if let Some(d) = &declared {
                        self.expect(&got, d, c.pos, "const initializer");
                    }
                }
                _ => {}
            }
        }
    }

    fn named_self_ty(&self, name: &str) -> Ty {
        if self.enums.contains_key(name) {
            Ty::Enum(name.to_string(), vec![])
        } else {
            Ty::Struct(name.to_string(), vec![])
        }
    }

    fn check_fn(&mut self, f: &FnDecl, extra_generics: &HashSet<String>) {
        self.check_fn_with_self(f, extra_generics, None);
    }

    fn check_fn_with_self(
        &mut self,
        f: &FnDecl,
        extra_generics: &HashSet<String>,
        self_ty: Option<Ty>,
    ) {
        let saved = self.type_params.clone();
        for g in extra_generics {
            self.type_params.insert(g.clone());
        }
        for (g, _) in &f.generics {
            self.type_params.insert(g.clone());
        }
        self.push_scope();
        if let Some(st) = &self_ty {
            self.declare("self", st.clone());
        }
        for p in &f.params {
            if p.is_self {
                if let Some(st) = &self_ty {
                    self.declare("self", st.clone());
                }
            } else {
                let ty =
                    p.ty.as_ref()
                        .map(|t| self.resolve(t))
                        .unwrap_or(Ty::Unknown);
                self.declare(&p.name, ty);
            }
        }
        if let Some(v) = &f.variadic {
            let elem =
                v.ty.as_ref()
                    .map(|t| self.resolve(t))
                    .unwrap_or(Ty::Unknown);
            self.declare(&v.name, Ty::List(Box::new(elem)));
        }
        let mut ret = f.ret.as_ref().map(|t| self.resolve(t)).unwrap_or(Ty::Unit);
        if f.is_async {
            ret = ret.strip_future();
        }
        self.ret_stack.push(ret.clone());
        let body_ty = self.check_block(&f.body);
        // The trailing expression is an implicit return.
        if !matches!(ret, Ty::Unit) {
            self.expect(&body_ty, &ret, f.body.pos, "function return value");
        }
        self.ret_stack.pop();
        self.pop_scope();
        self.type_params = saved;
    }

    // ---- scopes ----------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    fn declare(&mut self, name: &str, ty: Ty) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.to_string(), ty);
        }
    }
    fn lookup(&self, name: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            if let Some(t) = s.get(name) {
                return Some(t.clone());
            }
        }
        if let Some(t) = self.consts.get(name) {
            return Some(t.clone());
        }
        None
    }

    fn err(&mut self, pos: Pos, msg: impl Into<String>) {
        self.errors
            .push(Diagnostic::new(Phase::Check, pos, msg.into()));
    }

    /// Report when `got` is not assignable to `want`. Lenient by design: an
    /// `Unknown`, `Param`, or numeric-literal type never triggers a complaint.
    fn expect(&mut self, got: &Ty, want: &Ty, pos: Pos, ctx: &str) {
        if !assignable(got, want) {
            self.err(
                pos,
                format!(
                    "type mismatch in {}: expected `{}`, found `{}`",
                    ctx,
                    display_ty(want),
                    display_ty(got)
                ),
            );
        }
    }

    // ---- statements ------------------------------------------------------

    fn check_block(&mut self, b: &Block) -> Ty {
        self.push_scope();
        for s in &b.stmts {
            self.check_stmt(s);
        }
        let ty = match &b.tail {
            Some(e) => self.infer(e),
            None => Ty::Unit,
        };
        self.pop_scope();
        ty
    }

    fn check_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Let {
                pattern,
                ty,
                value,
                pos,
                ..
            } => {
                let declared = ty.as_ref().map(|t| self.resolve(t));
                let mut got = self.infer(value);
                if let Some(d) = &declared {
                    self.expect(&got, d, *pos, "let binding");
                    got = d.clone();
                }
                self.bind_pattern(pattern, &got);
            }
            Stmt::Expr(e) => {
                self.infer(e);
            }
            Stmt::Return(opt, pos) => {
                let ret = self.ret_stack.last().cloned().unwrap_or(Ty::Unit);
                let got = match opt {
                    Some(e) => self.infer(e),
                    None => Ty::Unit,
                };
                self.expect(&got, &ret, *pos, "return value");
            }
            Stmt::Break(opt, _) => {
                let got = match opt {
                    Some(e) => self.infer(e),
                    None => Ty::Unit,
                };
                if let Some(breaks) = self.loop_breaks.last_mut() {
                    breaks.push(got);
                }
            }
            Stmt::Continue(_) => {}
            Stmt::Item(item) => {
                if let Item::Fn(f) = item {
                    let sig = self.fn_sig(f, &self.type_params.clone());
                    self.fns.insert(f.name.clone(), sig);
                    self.check_fn(f, &self.type_params.clone());
                }
            }
        }
    }

    /// Bind the names introduced by an irrefutable pattern, threading the known
    /// type of the matched value into each binding.
    fn bind_pattern(&mut self, p: &Pattern, ty: &Ty) {
        match p {
            Pattern::Binding(n) => self.declare(n, ty.clone()),
            Pattern::At(n, sub) => {
                self.declare(n, ty.clone());
                self.bind_pattern(sub, ty);
            }
            Pattern::Tuple(ps) => {
                let elems = match ty {
                    Ty::Tuple(ts) if ts.len() == ps.len() => ts.clone(),
                    _ => vec![Ty::Unknown; ps.len()],
                };
                for (p, t) in ps.iter().zip(elems.iter()) {
                    self.bind_pattern(p, t);
                }
            }
            Pattern::List { items, rest } => {
                let elem = elem_ty(ty);
                for p in items {
                    self.bind_pattern(p, &elem);
                }
                if let Some(r) = rest {
                    if !r.is_empty() {
                        self.declare(r, Ty::List(Box::new(elem)));
                    }
                }
            }
            Pattern::Variant { path, args } => {
                let payloads = self.variant_payloads(path, ty, args.len());
                for (p, t) in args.iter().zip(payloads.iter()) {
                    self.bind_pattern(p, t);
                }
            }
            Pattern::Struct { name, fields } => {
                for f in fields {
                    let fty = self
                        .field_ty(&Ty::Struct(name.clone(), vec![]), f)
                        .unwrap_or(Ty::Unknown);
                    self.declare(f, fty);
                }
            }
            Pattern::Typed { binding, ty: te } => {
                let narrowed = self.resolve(te);
                self.declare(binding, narrowed);
            }
            Pattern::Or(ps) => {
                for p in ps {
                    self.bind_pattern(p, ty);
                }
            }
            _ => {}
        }
    }

    /// The payload types of an enum variant pattern. `Option`/`Result` are known
    /// exactly; user enums keep payloads as `Unknown` (the AST records arity but
    /// not the element types of a variant).
    fn variant_payloads(&self, path: &[String], scrut: &Ty, arity: usize) -> Vec<Ty> {
        let variant = path.last().map(|s| s.as_str()).unwrap_or("");
        let inner = match scrut {
            Ty::Enum(_, args) => args.first().cloned().unwrap_or(Ty::Unknown),
            _ => Ty::Unknown,
        };
        match variant {
            "Some" | "Ok" => vec![inner],
            "Err" => vec![Ty::Str],
            "None" => vec![],
            _ => vec![Ty::Unknown; arity],
        }
    }

    // ---- expression inference -------------------------------------------

    fn infer(&mut self, e: &Expr) -> Ty {
        match &e.kind {
            ExprKind::Int(_) => Ty::IntLit,
            ExprKind::Float(_) => Ty::FloatLit,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Char(_) => Ty::Char,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Nil => Ty::Nil,
            ExprKind::FStr(parts) => {
                for p in parts {
                    if let FStrPart::Expr { expr, .. } = p {
                        self.infer(expr);
                    }
                }
                Ty::Str
            }
            ExprKind::Ident(name) => self.infer_ident(name),
            ExprKind::SelfExpr => self.lookup("self").unwrap_or(Ty::Unknown),
            ExprKind::Path(_) => Ty::Unknown,
            ExprKind::Unary { op, expr } => self.infer_unary(*op, expr, e.pos),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs, e.pos),
            ExprKind::Coalesce { lhs, rhs } => self.infer_coalesce(lhs, rhs, e.pos),
            ExprKind::Assign { target, value, .. } => {
                self.infer(target);
                self.infer(value);
                Ty::Unit
            }
            ExprKind::Cast { expr, ty } => {
                self.infer(expr);
                self.resolve(ty)
            }
            ExprKind::Call { callee, args } => self.infer_call(callee, args, e.pos),
            ExprKind::MethodCall {
                recv,
                optional,
                method,
                type_args,
                args,
            } => self.infer_method(recv, *optional, method, type_args, args, e.pos),
            ExprKind::Field {
                recv,
                optional,
                name,
            } => self.infer_field(recv, *optional, name, e.pos),
            ExprKind::Index { recv, index } => self.infer_index(recv, index),
            ExprKind::Tuple(xs) => {
                // `()` is the unit value, not a zero-element tuple type.
                if xs.is_empty() {
                    Ty::Unit
                } else {
                    Ty::Tuple(xs.iter().map(|x| self.infer(x)).collect())
                }
            }
            ExprKind::List(xs) => {
                let mut elem = Ty::Unknown;
                for x in xs {
                    let t = self.infer(x);
                    elem = join(&elem, &t);
                }
                Ty::List(Box::new(elem))
            }
            ExprKind::ListRepeat { value, count } => {
                let elem = self.infer(value);
                self.infer(count);
                Ty::List(Box::new(elem))
            }
            ExprKind::Map(entries) => {
                if entries.is_empty() {
                    // `{}` is an empty map or set; either is acceptable.
                    return Ty::Unknown;
                }
                let mut k = Ty::Unknown;
                let mut v = Ty::Unknown;
                for (ke, ve) in entries {
                    k = join(&k, &self.infer(ke));
                    v = join(&v, &self.infer(ve));
                }
                Ty::Map(Box::new(k), Box::new(v))
            }
            ExprKind::Set(xs) => {
                let mut elem = Ty::Unknown;
                for x in xs {
                    elem = join(&elem, &self.infer(x));
                }
                Ty::Set(Box::new(elem))
            }
            ExprKind::StructLit {
                name,
                fields,
                spread,
            } => self.infer_struct_lit(name, fields, spread, e.pos),
            ExprKind::Block(b) => self.check_block(b),
            ExprKind::If { cond, then, els } => self.infer_if(cond, then, els, e.pos),
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms, e.pos),
            ExprKind::Loop { body } => {
                self.loop_breaks.push(Vec::new());
                self.check_block(body);
                let breaks = self.loop_breaks.pop().unwrap_or_default();
                let mut t = Ty::Unknown;
                let mut any = false;
                for b in &breaks {
                    t = join(&t, b);
                    any = true;
                }
                if any {
                    t
                } else {
                    Ty::Unit
                }
            }
            ExprKind::While { cond, body } => {
                let c = self.infer(cond);
                self.expect_bool(&c, cond.pos, "while condition");
                self.check_block(body);
                Ty::Unit
            }
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => {
                let scrut = self.infer(expr);
                self.push_scope();
                self.bind_pattern_refutable(pattern, &scrut);
                self.check_block(body);
                self.pop_scope();
                Ty::Unit
            }
            ExprKind::For {
                pattern,
                iter,
                body,
            } => {
                let it = self.infer(iter);
                let elem = elem_ty(&it);
                self.push_scope();
                self.bind_pattern(pattern, &elem);
                self.check_block(body);
                self.pop_scope();
                Ty::Unit
            }
            ExprKind::Range { start, end, .. } => {
                let s = self.infer(start);
                let en = self.infer(end);
                let elem = num_join(&s, &en).unwrap_or(Ty::IntLit);
                Ty::Range(Box::new(elem))
            }
            ExprKind::Closure { params, body, .. } => self.infer_closure(params, body, None),
            ExprKind::Try(inner) => self.infer_try(inner, e.pos),
            ExprKind::Await(inner) => {
                let t = self.infer(inner);
                t.strip_future()
            }
            ExprKind::Spawn(body) => {
                let t = self.check_block(body);
                Ty::Future(Box::new(t))
            }
            ExprKind::Unsafe(body) => {
                self.unsafe_depth += 1;
                let t = self.check_block(body);
                self.unsafe_depth -= 1;
                t
            }
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                let t = self.check_block(body);
                for c in catches {
                    self.push_scope();
                    if let Some(b) = &c.binding {
                        self.declare(b, Ty::Unknown);
                    }
                    self.check_block(&c.body);
                    self.pop_scope();
                }
                if let Some(f) = finally {
                    self.check_block(f);
                }
                t
            }
        }
    }

    fn bind_pattern_refutable(&mut self, p: &Pattern, scrut: &Ty) {
        // For `if let` / `while let`, narrow `Some(x)`/`Ok(x)` to the payload.
        self.bind_pattern(p, scrut);
    }

    fn infer_ident(&mut self, name: &str) -> Ty {
        if let Some(t) = self.lookup(name) {
            return t;
        }
        match name {
            "None" => Ty::option(Ty::Unknown),
            "self" => self.lookup("self").unwrap_or(Ty::Unknown),
            _ => {
                if let Some(sig) = self.fns.get(name) {
                    return Ty::Fn(sig.params.clone(), Box::new(sig.ret.clone()));
                }
                Ty::Unknown
            }
        }
    }

    fn infer_unary(&mut self, op: UnOp, expr: &Expr, pos: Pos) -> Ty {
        let t = self.infer(expr);
        match op {
            UnOp::Not => {
                self.expect_bool(&t, pos, "operand of `!`");
                Ty::Bool
            }
            UnOp::Neg => {
                if !t.is_numeric() && !t.is_unknown() && !matches!(t, Ty::Param(_)) {
                    self.err(
                        pos,
                        format!("cannot negate a value of type `{}`", display_ty(&t)),
                    );
                }
                t
            }
            UnOp::BitNot => {
                if !t.is_int() && !t.is_unknown() && !matches!(t, Ty::Param(_)) {
                    self.err(
                        pos,
                        format!("`~` requires an integer, found `{}`", display_ty(&t)),
                    );
                }
                t
            }
            UnOp::Ref | UnOp::RefMut => Ty::Ref(Box::new(t)),
            UnOp::RawRef => Ty::Ptr(Box::new(t)),
            UnOp::Deref => match t {
                // Dereferencing a raw pointer is only allowed inside `unsafe`.
                Ty::Ptr(inner) => {
                    if self.unsafe_depth == 0 {
                        self.err(
                            pos,
                            "dereferencing a raw pointer requires an `unsafe` block",
                        );
                    }
                    *inner
                }
                Ty::Ref(inner) => *inner,
                other => other,
            },
        }
    }

    fn infer_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, pos: Pos) -> Ty {
        let l = self.infer(lhs);
        let r = self.infer(rhs);
        use BinOp::*;

        // Pointer arithmetic (Section 11 / spec Section 4).
        // `*T + integer → *T`, `*mut T ± integer → *mut T`, `q - p → isize`.
        // The offset may be any integer kind, an unresolved integer literal,
        // Unknown, or a generic Param – it does NOT have to match the pointed-to
        // type. Non-integer offsets produce a targeted diagnostic.
        if matches!(op, Add | Sub) {
            // `q - p` → element distance (isize)
            if matches!((&l, &r), (Ty::Ptr(_), Ty::Ptr(_))) {
                return Ty::Int(IntKind::Isize);
            }
            // `p ± n` → same pointer type
            if matches!(&l, Ty::Ptr(_)) {
                let offset_ok =
                    r.is_int() || r.is_unknown() || matches!(&r, Ty::Param(_));
                if !offset_ok {
                    self.err(
                        pos,
                        format!(
                            "pointer arithmetic offset must be an integer, found `{}`; \
                             use an integer type (`i32`, `i64`, `usize`, …)",
                            display_ty(&r)
                        ),
                    );
                }
                return l;
            }
            // `n + p` → same pointer type (commutative add only)
            if matches!(&r, Ty::Ptr(_)) && matches!(op, Add) {
                let offset_ok =
                    l.is_int() || l.is_unknown() || matches!(&l, Ty::Param(_));
                if !offset_ok {
                    self.err(
                        pos,
                        format!(
                            "pointer arithmetic offset must be an integer, found `{}`; \
                             use an integer type (`i32`, `i64`, `usize`, …)",
                            display_ty(&l)
                        ),
                    );
                }
                return r;
            }
        }

        match op {
            Add | Sub | Mul | Div | Rem => {
                // `+` also concatenates strings.
                if matches!(op, Add) && (matches!(l, Ty::Str) || matches!(r, Ty::Str)) {
                    if !is_str_like(&l) || !is_str_like(&r) {
                        self.err(
                            pos,
                            format!("cannot add `{}` and `{}`", display_ty(&l), display_ty(&r)),
                        );
                    }
                    return Ty::Str;
                }
                match num_join(&l, &r) {
                    Some(t) => t,
                    None => {
                        self.err(
                            pos,
                            format!(
                                "arithmetic operands must share a type: `{}` and `{}` differ; add an explicit `as` cast",
                                display_ty(&l),
                                display_ty(&r)
                            ),
                        );
                        Ty::Unknown
                    }
                }
            }
            Pow => {
                // `**` always yields f64 (Section 4).
                Ty::Float(FloatKind::F64)
            }
            BitAnd | BitOr | BitXor | Shl | Shr => {
                let lok = l.is_int() || l.is_unknown() || matches!(l, Ty::Param(_));
                let rok = r.is_int() || r.is_unknown() || matches!(r, Ty::Param(_));
                if !lok || !rok {
                    self.err(
                        pos,
                        format!(
                            "bitwise operator requires integers, found `{}` and `{}`",
                            display_ty(&l),
                            display_ty(&r)
                        ),
                    );
                    return Ty::Unknown;
                }
                // Shifts keep the left type; the others must share a type.
                if matches!(op, Shl | Shr) {
                    return concrete_int(&l);
                }
                num_join(&l, &r).unwrap_or(Ty::IntLit)
            }
            Eq | Ne | Lt | Gt | Le | Ge => Ty::Bool,
            And | Or => {
                self.expect_bool(&l, lhs.pos, "logical operand");
                self.expect_bool(&r, rhs.pos, "logical operand");
                Ty::Bool
            }
        }
    }

    fn infer_coalesce(&mut self, lhs: &Expr, rhs: &Expr, pos: Pos) -> Ty {
        let l = self.infer(lhs);
        let r = self.infer(rhs);
        // `??` operates on a bare optional `T | nil` (Section 4).
        if !l.is_optional() && !matches!(l, Ty::Param(_)) {
            self.err(
                pos,
                format!(
                    "`??` expects an optional `T | nil` on the left, found `{}`",
                    display_ty(&l)
                ),
            );
        }
        join(&l.strip_nil(), &r)
    }

    /// Aliasing xor mutability (Section 11): within a single call's argument
    /// list a value may be borrowed by many `&T` or by exactly one `&mut T`,
    /// never both. This is a conservative, intra-call check (it does not track
    /// borrows across statements), so it never fires on borrow-free code.
    fn check_borrow_conflicts(&mut self, args: &[Expr], pos: Pos) {
        let mut roots: Vec<(String, bool)> = Vec::new();
        for a in args {
            if let Some(b) = borrow_root(a) {
                roots.push(b);
            }
        }
        let mut reported: HashSet<String> = HashSet::new();
        for (name, mutable) in &roots {
            if !mutable {
                continue;
            }
            let others = roots.iter().filter(|(n, _)| n == name).count();
            if others > 1 && reported.insert(name.clone()) {
                self.err(
                    pos,
                    format!(
                        "cannot borrow `{}` mutably while it is also borrowed in the same call (aliasing xor mutability)",
                        name
                    ),
                );
            }
        }
    }

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], pos: Pos) -> Ty {
        self.check_borrow_conflicts(args, pos);
        // Enum constructors carry their payload type.
        if let ExprKind::Ident(name) = &callee.kind {
            match name.as_str() {
                "Some" => {
                    let a = args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown);
                    for a in args.iter().skip(1) {
                        self.infer(a);
                    }
                    return Ty::option(a);
                }
                "Ok" => {
                    let a = args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown);
                    for a in args.iter().skip(1) {
                        self.infer(a);
                    }
                    return Ty::result(a);
                }
                "Err" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::result(Ty::Unknown);
                }
                "str" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Str;
                }
                "len" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Int(IntKind::Usize);
                }
                "idiv" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Int(IntKind::I64);
                }
                "to_hex" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Str;
                }
                "from_hex" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::result(Ty::List(Box::new(Ty::Int(IntKind::U8))));
                }
                // Heap allocation (Section 11).
                "alloc" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Ptr(Box::new(Ty::Int(IntKind::U8)));
                }
                "dealloc" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Unit;
                }
                _ => {}
            }
            // A user function with a recorded signature.
            if let Some(sig) = self.fns.get(name).cloned() {
                self.check_call_args(&sig, args);
                self.check_bounds(&sig, args, pos);
                return self.instantiate_ret(&sig, args);
            }
        }

        // Associated calls like `Point.new(...)` / `TlsRecord.decode(...)`.
        let cty = self.infer(callee);
        for a in args {
            self.infer(a);
        }
        match cty {
            Ty::Fn(_, ret) => *ret,
            other if other.is_unknown() => Ty::Unknown,
            _ => Ty::Unknown,
        }
    }

    fn check_call_args(&mut self, sig: &FnSig, args: &[Expr]) {
        for (i, a) in args.iter().enumerate() {
            let expected = sig.params.get(i).cloned();
            if let ExprKind::Closure { params, body, .. } = &a.kind {
                self.infer_closure(params, body, expected.as_ref());
            } else {
                self.infer(a);
            }
        }
    }

    /// Section 9: a generic bound `T: Iface` is satisfied only when an explicit
    /// `impl Iface for ConcreteType` exists (primitives satisfy the builtin
    /// interfaces automatically).
    fn check_bounds(&mut self, sig: &FnSig, args: &[Expr], pos: Pos) {
        if sig.generics.iter().all(|(_, b)| b.is_empty()) {
            return;
        }
        let mut bindings: HashMap<String, Ty> = HashMap::new();
        for (i, p) in sig.params.iter().enumerate() {
            if let (Ty::Param(name), Some(arg)) = (p, args.get(i)) {
                let at = self.infer(arg);
                bindings.entry(name.clone()).or_insert(at);
            }
        }
        for (name, bounds) in &sig.generics {
            let Some(concrete) = bindings.get(name) else {
                continue;
            };
            let tyname = match concrete {
                Ty::Struct(n, _) | Ty::Enum(n, _) => n.clone(),
                _ => continue, // primitives, unknown, params: assume satisfied
            };
            for b in bounds {
                if builtin_interface(b) {
                    continue;
                }
                if !self.satisfies(&tyname, b) {
                    self.err(
                        pos,
                        format!("`{}` does not implement interface `{}`", tyname, b),
                    );
                }
            }
        }
    }

    fn satisfies(&self, tyname: &str, iface: &str) -> bool {
        if self
            .impls
            .contains(&(iface.to_string(), tyname.to_string()))
        {
            return true;
        }
        // A combined marker interface (`interface Codec: Encode + Decode {}`) is
        // satisfied when all of its component interfaces are. A plain interface
        // with no super-interfaces requires its own explicit `impl`.
        if let Some(info) = self.interfaces.get(iface) {
            if !info.supers.is_empty() {
                return info.supers.iter().all(|s| self.satisfies(tyname, s));
            }
        }
        false
    }

    fn instantiate_ret(&mut self, sig: &FnSig, args: &[Expr]) -> Ty {
        // Infer generic bindings from argument types, then substitute.
        let mut bindings: HashMap<String, Ty> = HashMap::new();
        for (i, p) in sig.params.iter().enumerate() {
            if let Some(arg) = args.get(i) {
                let at = self.infer(arg);
                collect_param_bindings(p, &at, &mut bindings);
            }
        }
        subst(&sig.ret, &bindings)
    }

    fn infer_method(
        &mut self,
        recv: &Expr,
        optional: bool,
        method: &str,
        _type_args: &[TypeExpr],
        args: &[Expr],
        _pos: Pos,
    ) -> Ty {
        self.check_borrow_conflicts(args, _pos);
        // `module.fn(...)` (io, fs, os, math, json, bytes, crypto, net, ...).
        if let ExprKind::Ident(modname) = &recv.kind {
            if self.lookup(modname).is_none() && is_module(modname) {
                for a in args {
                    self.infer(a);
                }
                return module_fn_ret(modname, method);
            }
        }

        let rty = self.infer(recv);
        let base = if optional {
            rty.strip_nil()
        } else {
            rty.clone()
        };

        // A user method on a struct/enum.
        if let Ty::Struct(name, _) | Ty::Enum(name, _) = &base {
            if let Some(m) = self.methods.get(&(name.clone(), method.to_string())) {
                let ret = m.sig.ret.clone();
                for a in args {
                    self.infer(a);
                }
                return wrap_optional(ret, optional);
            }
        }

        // Builtin methods on str / List / Map / Set / Option / Result.
        let (param_tys, ret) = builtin_method_sig(&base, method);
        for (i, a) in args.iter().enumerate() {
            let expected = param_tys.get(i).cloned();
            if let ExprKind::Closure { params, body, .. } = &a.kind {
                self.infer_closure(params, body, expected.as_ref());
            } else {
                self.infer(a);
            }
        }
        wrap_optional(ret, optional)
    }

    fn infer_field(&mut self, recv: &Expr, optional: bool, name: &str, _pos: Pos) -> Ty {
        // Module constants: `math.pi`, `math.e`, `math.inf`.
        if let ExprKind::Ident(modname) = &recv.kind {
            if self.lookup(modname).is_none() {
                if modname == "math" && matches!(name, "pi" | "e" | "inf") {
                    return Ty::Float(FloatKind::F64);
                }
                // `Enum.Variant` / `Type.assoc` accessed through an identifier.
                if let Some(t) = self.type_member(modname, name) {
                    return t;
                }
                if is_module(modname) {
                    return Ty::Unknown;
                }
            }
        }
        let rty = self.infer(recv);
        let base = if optional { rty.strip_nil() } else { rty };
        let result = self.field_ty(&base, name).unwrap_or(Ty::Unknown);
        wrap_optional(result, optional)
    }

    /// `Type.member` where `Type` is a known struct or enum name.
    fn type_member(&self, tyname: &str, member: &str) -> Option<Ty> {
        if let Some(info) = self.enums.get(tyname) {
            if let Some(v) = info.variants.iter().find(|v| v.name == member) {
                return Some(match &v.kind {
                    VariantKind::Unit => Ty::Enum(tyname.to_string(), vec![]),
                    VariantKind::Tuple(n) => Ty::Fn(
                        vec![Ty::Unknown; *n],
                        Box::new(Ty::Enum(tyname.to_string(), vec![])),
                    ),
                    VariantKind::Struct(_) => Ty::Enum(tyname.to_string(), vec![]),
                });
            }
        }
        if let Some(m) = self.methods.get(&(tyname.to_string(), member.to_string())) {
            return Some(Ty::Fn(m.sig.params.clone(), Box::new(m.sig.ret.clone())));
        }
        None
    }

    fn field_ty(&self, base: &Ty, name: &str) -> Option<Ty> {
        match base {
            Ty::Struct(sname, args) => {
                let info = self.structs.get(sname)?;
                let (_, fty) = info.fields.iter().find(|(fname, _)| fname == name)?;
                let resolved = self.resolve_in(fty, &info.generics.iter().cloned().collect());
                let bindings: HashMap<String, Ty> = info
                    .generics
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                Some(subst(&resolved, &bindings))
            }
            Ty::Tuple(ts) => name.parse::<usize>().ok().and_then(|i| ts.get(i).cloned()),
            Ty::Ref(inner) => self.field_ty(inner, name),
            _ => None,
        }
    }

    fn infer_index(&mut self, recv: &Expr, index: &Expr) -> Ty {
        let r = self.infer(recv);
        let is_range = matches!(index.kind, ExprKind::Range { .. });
        self.infer(index);
        match r {
            Ty::List(e) | Ty::Array(e, _) | Ty::Slice(e) => {
                if is_range {
                    Ty::List(e)
                } else {
                    *e
                }
            }
            Ty::Map(_, v) => *v,
            Ty::Str => {
                if is_range {
                    Ty::Str
                } else {
                    Ty::Char
                }
            }
            _ => Ty::Unknown,
        }
    }

    fn infer_struct_lit(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        spread: &Option<Box<Expr>>,
        pos: Pos,
    ) -> Ty {
        let known: Option<(Vec<String>, Vec<(String, TypeExpr)>)> = self
            .structs
            .get(name)
            .map(|s| (s.generics.clone(), s.fields.clone()));
        if let Some((generics, decl_fields)) = known {
            let gset: HashSet<String> = generics.iter().cloned().collect();
            for (fname, fexpr) in fields {
                let got = self.infer(fexpr);
                match decl_fields.iter().find(|(dn, _)| dn == fname) {
                    Some((_, fty)) => {
                        let want = self.resolve_in(fty, &gset);
                        self.expect(&got, &want, fexpr.pos, &format!("field `{}`", fname));
                    }
                    None => self.err(
                        fexpr.pos,
                        format!("struct `{}` has no field `{}`", name, fname),
                    ),
                }
            }
            if let Some(s) = spread {
                self.infer(s);
            } else {
                // Every field must be supplied when there is no `..spread`.
                for (dn, _) in &decl_fields {
                    if !fields.iter().any(|(fname, _)| fname == dn) {
                        self.err(pos, format!("missing field `{}` in `{}` literal", dn, name));
                    }
                }
            }
            Ty::Struct(name.to_string(), vec![Ty::Unknown; generics.len()])
        } else {
            for (_, fexpr) in fields {
                self.infer(fexpr);
            }
            if let Some(s) = spread {
                self.infer(s);
            }
            Ty::Unknown
        }
    }

    fn infer_if(&mut self, cond: &Expr, then: &Block, els: &Option<Box<Expr>>, _pos: Pos) -> Ty {
        let c = self.infer(cond);
        self.expect_bool(&c, cond.pos, "if condition");
        let then_ty = self.check_block(then);
        match els {
            Some(e) => {
                let else_ty = self.infer(e);
                if !unifies(&then_ty, &else_ty) {
                    self.err(
                        then.pos,
                        format!(
                            "`if` and `else` branches have incompatible types: `{}` and `{}`",
                            display_ty(&then_ty),
                            display_ty(&else_ty)
                        ),
                    );
                }
                join(&then_ty, &else_ty)
            }
            // An `if` without `else` is a statement; its value is unit.
            None => Ty::Unit,
        }
    }

    fn infer_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], pos: Pos) -> Ty {
        let scrut = self.infer(scrutinee);
        let mut result = Ty::Unknown;
        let mut first = true;
        let mut prev: Option<Ty> = None;
        for arm in arms {
            self.push_scope();
            self.bind_pattern(&arm.pattern, &scrut);
            if let Some(g) = &arm.guard {
                let gt = self.infer(g);
                self.expect_bool(&gt, g.pos, "match guard");
            }
            let arm_ty = self.infer(&arm.body);
            self.pop_scope();
            if !first {
                if let Some(p) = &prev {
                    if !unifies(p, &arm_ty) {
                        self.err(
                            arm.body.pos,
                            format!(
                                "match arms have incompatible types: `{}` and `{}`",
                                display_ty(p),
                                display_ty(&arm_ty)
                            ),
                        );
                    }
                }
            }
            result = join(&result, &arm_ty);
            prev = Some(arm_ty);
            first = false;
        }
        self.check_exhaustive(&scrut, arms, pos);
        result
    }

    /// Section 7: `match` must be exhaustive. Enums need every variant (or a
    /// wildcard); `bool` needs both values; a union needs every member type;
    /// open scrutinees (integers, strings, tuples) need a `_` arm.
    fn check_exhaustive(&mut self, scrut: &Ty, arms: &[MatchArm], pos: Pos) {
        if scrut.is_unknown() || matches!(scrut, Ty::Param(_)) {
            return;
        }
        // A guardless catch-all settles exhaustiveness immediately.
        let has_catch_all = arms.iter().any(|a| {
            a.guard.is_none() && matches!(a.pattern, Pattern::Wildcard | Pattern::Binding(_))
        });
        if has_catch_all {
            return;
        }
        match scrut {
            Ty::Enum(name, _) => {
                let variants = self.enum_variant_names(name);
                let Some(all) = variants else { return };
                let mut covered: HashSet<String> = HashSet::new();
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    collect_covered_variants(&a.pattern, &mut covered);
                }
                let missing: Vec<String> = all
                    .iter()
                    .filter(|v| !covered.contains(*v))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    self.err(
                        pos,
                        format!(
                            "non-exhaustive match: missing variant(s) {}",
                            missing.join(", ")
                        ),
                    );
                }
            }
            Ty::Bool => {
                let mut t = false;
                let mut f = false;
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    if let Pattern::Bool(b) = a.pattern {
                        if b {
                            t = true
                        } else {
                            f = true
                        }
                    }
                }
                if !(t && f) {
                    self.err(
                        pos,
                        "non-exhaustive match: `bool` needs both `true` and `false`",
                    );
                }
            }
            Ty::Union(members) => {
                let mut covered: HashSet<String> = HashSet::new();
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    if let Pattern::Typed { ty, .. } = &a.pattern {
                        covered.insert(display_ty(&self.resolve(ty)));
                    }
                    if matches!(a.pattern, Pattern::Nil) {
                        covered.insert("nil".into());
                    }
                }
                let missing: Vec<String> = members
                    .iter()
                    .map(display_ty)
                    .filter(|m| !covered.contains(m))
                    .collect();
                if !missing.is_empty() {
                    self.err(
                        pos,
                        format!(
                            "non-exhaustive match: union member(s) {} not narrowed",
                            missing.join(", ")
                        ),
                    );
                }
            }
            _ => {
                self.err(
                    pos,
                    "non-exhaustive match: add a `_` arm to cover the remaining cases",
                );
            }
        }
    }

    fn enum_variant_names(&self, name: &str) -> Option<Vec<String>> {
        match name {
            "Option" => Some(vec!["Some".into(), "None".into()]),
            "Result" => Some(vec!["Ok".into(), "Err".into()]),
            _ => self
                .enums
                .get(name)
                .map(|e| e.variants.iter().map(|v| v.name.clone()).collect()),
        }
    }

    fn infer_closure(&mut self, params: &[Param], body: &Expr, expected: Option<&Ty>) -> Ty {
        let expected_params: Vec<Ty> = match expected {
            Some(Ty::Fn(ps, _)) => ps.clone(),
            _ => Vec::new(),
        };
        self.push_scope();
        let mut ptys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = if let Some(t) = &p.ty {
                self.resolve(t)
            } else if let Some(t) = expected_params.get(i) {
                // Closure parameters receive the element, not a reference to it.
                strip_ref(t)
            } else {
                Ty::Unknown
            };
            self.declare(&p.name, ty.clone());
            ptys.push(ty);
        }
        let ret = self.infer(body);
        self.pop_scope();
        Ty::Fn(ptys, Box::new(ret))
    }

    fn infer_try(&mut self, inner: &Expr, pos: Pos) -> Ty {
        let t = self.infer(inner);
        // `?` may appear only inside a function returning Result or Option.
        match self.ret_stack.last() {
            Some(Ty::Enum(n, _)) if n == "Result" || n == "Option" => {}
            Some(Ty::Unknown) | None => {}
            Some(other) => {
                let other = other.clone();
                self.err(
                    pos,
                    format!(
                        "`?` can only be used in a function returning `Result` or `Option`, not `{}`",
                        display_ty(&other)
                    ),
                );
            }
        }
        match t {
            Ty::Enum(n, args) if n == "Result" || n == "Option" => {
                args.into_iter().next().unwrap_or(Ty::Unknown)
            }
            other => other,
        }
    }

    fn check_conformance(&mut self, b: &ImplBlock, iface: &str) {
        // The interface must exist and its super-interfaces must also be
        // implemented for the type (nominal, Section 9).
        if let Some(info) = self.interfaces.get(iface).map(|i| InterfaceInfo {
            supers: i.supers.clone(),
        }) {
            for s in &info.supers {
                if !self.satisfies(&b.ty, s) {
                    self.err(
                        b.pos,
                        format!(
                            "`{}` implements `{}` but not its super-interface `{}`",
                            b.ty, iface, s
                        ),
                    );
                }
            }
        }
    }

    fn expect_bool(&mut self, t: &Ty, pos: Pos, ctx: &str) {
        let ok = matches!(t, Ty::Bool | Ty::Unknown) || matches!(t, Ty::Param(_));
        if !ok {
            self.err(
                pos,
                format!("{} must be `bool`, found `{}`", ctx, display_ty(t)),
            );
        }
    }
}

impl Ty {
    fn strip_future(self) -> Ty {
        match self {
            Ty::Future(inner) => *inner,
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Type relations
// ---------------------------------------------------------------------------

fn arg0(args: &[Ty]) -> Ty {
    args.first().cloned().unwrap_or(Ty::Unknown)
}
fn argn(args: &[Ty], n: usize) -> Ty {
    args.get(n).cloned().unwrap_or(Ty::Unknown)
}

fn is_str_like(t: &Ty) -> bool {
    matches!(t, Ty::Str | Ty::Unknown) || matches!(t, Ty::Param(_))
}

fn is_module(name: &str) -> bool {
    matches!(
        name,
        "io" | "fs" | "net" | "http" | "dns" | "tcp" | "bytes" | "crypto" | "json" | "os" | "math"
    )
}

fn strip_ref(t: &Ty) -> Ty {
    match t {
        Ty::Ref(inner) => (**inner).clone(),
        other => other.clone(),
    }
}

fn elem_ty(t: &Ty) -> Ty {
    match t {
        Ty::List(e) | Ty::Array(e, _) | Ty::Slice(e) | Ty::Set(e) | Ty::Range(e) => (**e).clone(),
        Ty::Map(k, v) => Ty::Tuple(vec![(**k).clone(), (**v).clone()]),
        Ty::Str => Ty::Char,
        Ty::Ref(inner) => elem_ty(inner),
        _ => Ty::Unknown,
    }
}

fn concrete_int(t: &Ty) -> Ty {
    match t {
        Ty::IntLit => Ty::Int(IntKind::I32),
        Ty::Int(k) => Ty::Int(*k),
        _ => t.clone(),
    }
}

fn normalize_union(members: Vec<Ty>) -> Ty {
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
fn num_join(a: &Ty, b: &Ty) -> Option<Ty> {
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
fn join(a: &Ty, b: &Ty) -> Ty {
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
fn unifies(a: &Ty, b: &Ty) -> bool {
    assignable(a, b) || assignable(b, a)
}

/// Is a value of type `from` usable where `to` is expected?
fn assignable(from: &Ty, to: &Ty) -> bool {
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
fn collect_param_bindings(param: &Ty, arg: &Ty, out: &mut HashMap<String, Ty>) {
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
fn subst(t: &Ty, bindings: &HashMap<String, Ty>) -> Ty {
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

fn wrap_optional(t: Ty, optional: bool) -> Ty {
    if optional && !t.is_optional() {
        Ty::option(t)
    } else {
        t
    }
}

/// If `e` is a borrow of a root variable (`&x`, `&mut x`, `&raw x`, `&arr[i]`),
/// return `(variable name, is the borrow mutable)`.
fn borrow_root(e: &Expr) -> Option<(String, bool)> {
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

fn collect_covered_variants(p: &Pattern, out: &mut HashSet<String>) {
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

// ---------------------------------------------------------------------------
// Standard-library signatures (reference Section 13)
// ---------------------------------------------------------------------------

/// Return type of a `module.fn(...)` call. Unmodeled entries fall back to
/// `Unknown` so the checker never rejects a valid stdlib call.
fn module_fn_ret(module: &str, func: &str) -> Ty {
    use FloatKind::F64;
    use IntKind::*;
    let list_u8 = || Ty::List(Box::new(Ty::Int(U8)));
    match (module, func) {
        ("io", "read_line") | ("io", "read_all") => Ty::Str,
        ("io", _) => Ty::Unit,
        ("os", "args") => Ty::List(Box::new(Ty::Str)),
        ("os", "env") => Ty::option(Ty::Str),
        ("os", "exit") => Ty::Never,
        ("os", "now") => Ty::Int(U64),
        ("os", "sleep") => Ty::Future(Box::new(Ty::Unit)),
        ("fs", "read") => Ty::result(Ty::Str),
        ("fs", "read_bytes") => Ty::result(list_u8()),
        ("fs", "write") | ("fs", "write_bytes") => Ty::result(Ty::Unit),
        ("json", "encode") | ("json", "pretty") => Ty::result(Ty::Str),
        ("json", "decode") => Ty::result(Ty::Unknown),
        ("bytes", "to_hex") | ("bytes", "to_base64") => Ty::Str,
        ("bytes", "from_hex") | ("bytes", "from_base64") => Ty::result(list_u8()),
        ("bytes", "compare") => Ty::Bool,
        ("crypto", "random_bytes") => list_u8(),
        ("crypto", "sha256") | ("crypto", "sha3_256") | ("crypto", "hmac_sha256") => {
            Ty::Array(Box::new(Ty::Int(U8)), Some(32))
        }
        ("crypto", "sha512") => Ty::Array(Box::new(Ty::Int(U8)), Some(64)),
        ("math", _) => Ty::Float(F64),
        _ => Ty::Unknown,
    }
}

/// `(parameter types, return type)` for a builtin method on a primitive or
/// collection. Parameter types matter mainly so closures get inferred argument
/// types; an unmodeled method yields `Unknown` and no parameter hints.
fn builtin_method_sig(recv: &Ty, method: &str) -> (Vec<Ty>, Ty) {
    use IntKind::Usize;
    let usize_t = Ty::Int(Usize);
    let unit = Ty::Unit;
    let boolean = Ty::Bool;

    // Methods shared by every type.
    match method {
        "len" => return (vec![], usize_t),
        "is_empty" => return (vec![], boolean),
        _ => {}
    }

    match recv {
        Ty::Str => match method {
            "contains" | "starts_with" | "ends_with" => (vec![Ty::Str], Ty::Bool),
            "to_upper" | "to_lower" | "trim" | "trim_start" | "trim_end" => (vec![], Ty::Str),
            "replace" => (vec![Ty::Str, Ty::Str], Ty::Str),
            "repeat" => (vec![usize_t], Ty::Str),
            "split" => (vec![Ty::Str], Ty::List(Box::new(Ty::Str))),
            "split_once" => (vec![Ty::Str], Ty::option(Ty::Tuple(vec![Ty::Str, Ty::Str]))),
            "chars" => (vec![], Ty::List(Box::new(Ty::Char))),
            "as_bytes" => (vec![], Ty::Slice(Box::new(Ty::Int(IntKind::U8)))),
            "parse" => (vec![], Ty::result(Ty::Unknown)),
            _ => (vec![], Ty::Unknown),
        },
        Ty::List(e) | Ty::Slice(e) | Ty::Array(e, _) => {
            let e = (**e).clone();
            match method {
                "push" => (vec![e], unit),
                "pop" | "first" | "last" => (vec![], Ty::option(e)),
                "contains" => (vec![Ty::Ref(Box::new(e))], Ty::Bool),
                "extend" => (vec![Ty::Slice(Box::new(e))], unit),
                "map" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Unknown))],
                    Ty::List(Box::new(Ty::Unknown)),
                ),
                "filter" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Bool))],
                    Ty::List(Box::new(e)),
                ),
                "reduce" => (
                    vec![
                        Ty::Unknown,
                        Ty::Fn(vec![Ty::Unknown, e], Box::new(Ty::Unknown)),
                    ],
                    Ty::Unknown,
                ),
                "sort_by" => (
                    vec![Ty::Fn(vec![e.clone(), e.clone()], Box::new(Ty::Bool))],
                    Ty::List(Box::new(e)),
                ),
                "group_by" => (
                    vec![Ty::Fn(vec![e.clone()], Box::new(Ty::Unknown))],
                    Ty::Map(Box::new(Ty::Unknown), Box::new(Ty::List(Box::new(e)))),
                ),
                "enumerate" => (
                    vec![],
                    Ty::List(Box::new(Ty::Tuple(vec![Ty::Int(Usize), e]))),
                ),
                "zip" => (
                    vec![Ty::List(Box::new(Ty::Unknown))],
                    Ty::List(Box::new(Ty::Tuple(vec![e, Ty::Unknown]))),
                ),
                "collect" => (vec![], Ty::Unknown),
                _ => (vec![], Ty::Unknown),
            }
        }
        Ty::Map(k, v) => {
            let (k, v) = ((**k).clone(), (**v).clone());
            match method {
                "get" => (vec![k], Ty::option(v)),
                "insert" => (vec![k, v], unit),
                "remove" => (vec![k], unit),
                "contains" | "contains_key" => (vec![k], Ty::Bool),
                "keys" => (vec![], Ty::List(Box::new(k))),
                "values" => (vec![], Ty::List(Box::new(v))),
                _ => (vec![], Ty::Unknown),
            }
        }
        Ty::Set(e) => {
            let e = (**e).clone();
            match method {
                "insert" => (vec![e], unit),
                "contains" => (vec![e], Ty::Bool),
                "remove" => (vec![e], unit),
                _ => (vec![], Ty::Unknown),
            }
        }
        Ty::Enum(n, args) if n == "Option" => {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            match method {
                "unwrap" => (vec![], inner),
                "unwrap_or" => (vec![inner.clone()], inner),
                "unwrap_or_else" => (vec![Ty::Fn(vec![], Box::new(inner.clone()))], inner),
                "is_some" | "is_none" => (vec![], Ty::Bool),
                "map" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::option(Ty::Unknown),
                ),
                "and_then" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::Unknown,
                ),
                _ => (vec![], Ty::Unknown),
            }
        }
        Ty::Enum(n, args) if n == "Result" => {
            let inner = args.first().cloned().unwrap_or(Ty::Unknown);
            match method {
                "unwrap" | "expect" => (vec![], inner),
                "unwrap_or" => (vec![inner.clone()], inner),
                "unwrap_or_else" => (vec![Ty::Fn(vec![Ty::Str], Box::new(inner.clone()))], inner),
                "is_ok" | "is_err" => (vec![], Ty::Bool),
                "map" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::result(Ty::Unknown),
                ),
                "map_err" => (
                    vec![Ty::Fn(vec![Ty::Str], Box::new(Ty::Str))],
                    Ty::result(inner),
                ),
                "and_then" => (
                    vec![Ty::Fn(vec![inner], Box::new(Ty::Unknown))],
                    Ty::Unknown,
                ),
                _ => (vec![], Ty::Unknown),
            }
        }
        _ => (vec![], Ty::Unknown),
    }
}

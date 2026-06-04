//! HIR — the typed, desugaring-target intermediate representation (Phase 2).
//!
//! HIR is still a tree (no CFG — that is MIR, Phase 3) and still generic
//! (monomorphization is MIR 3.2), but unlike the AST it is:
//!
//! * **Typed.** Every [`HExpr`] embeds its semantic [`Ty`], taken from the
//!   [`TypeTable`] the type checker produced — the back-end never re-infers.
//! * **`BindingId`-based.** Every local *use* is an [`HExprKind::Local`] carrying
//!   the unique [`BindingId`] name resolution ([`crate::checker`]) assigned, and
//!   every binding *site* (`let`, parameter, pattern binding, closure param)
//!   records that same id. Names and shadowing were resolved once, in Phase 2.2;
//!   HIR and everything after it work on ids alone.
//!
//! Subpart 2.3 defined the HIR and a near-1:1 lowering. Subpart **2.4** (this)
//! removes the surface sugar during lowering, so HIR has **no sugar**:
//!
//! * f-strings → `+`-concatenation of `Str` literals and [`HExprKind::Format`].
//! * `a ?? b` and `a?.x` → a `match` on `nil`.
//! * `e?` → a `match` that unwraps and early-returns (`Result`/`Option`).
//! * compound `x += e` → plain `x = x + e`.
//! * `while let P = e { … }` → `loop { match e { P => …, _ => break } }`.
//!
//! (`if let` is not in the surface grammar — the parser only accepts `while let`
//! — so there is nothing to desugar for it.) The desugarings introduce fresh
//! *synthetic* binding ids (see [`Lower::fresh`]) for their temporaries. The
//! typed `for` loop is kept as an HIR node; its per-iterable step is lowered in
//! MIR.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::Pos;
use crate::ty::*;
use crate::typeck::TypeTable;

// ---------------------------------------------------------------------------
// HIR
// ---------------------------------------------------------------------------

/// A lowered program: the typed, id-resolved counterpart of [`Program`].
pub(crate) struct Hir {
    pub structs: Vec<HStruct>,
    pub enums: Vec<HEnum>,
    pub consts: Vec<HConst>,
    pub fns: Vec<HFn>,
}

pub(crate) struct HStruct {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

pub(crate) struct HEnum {
    pub name: String,
    pub variants: Vec<HVariant>,
}

pub(crate) struct HVariant {
    pub name: String,
    pub kind: HVariantKind,
}

pub(crate) enum HVariantKind {
    Unit,
    Tuple(Vec<Ty>),
    Struct(Vec<(String, Ty)>),
}

pub(crate) struct HConst {
    pub name: String,
    pub ty: Ty,
    pub value: HExpr,
}

pub(crate) struct HFn {
    pub name: String,
    /// The nominal type this method belongs to (`impl Type`), or `None` for a
    /// free function.
    pub owner: Option<String>,
    pub self_kind: SelfKind,
    pub params: Vec<HParam>,
    pub variadic: Option<HParam>,
    pub ret: Ty,
    pub body: HBlock,
    pub is_async: bool,
    pub pos: Pos,
}

pub(crate) struct HParam {
    pub binding: BindingId,
    pub name: String,
    pub ty: Ty,
}

pub(crate) struct HBlock {
    pub stmts: Vec<HStmt>,
    pub tail: Option<Box<HExpr>>,
    pub pos: Pos,
}

pub(crate) enum HStmt {
    Let {
        pattern: HPattern,
        mutable: bool,
        ty: Ty,
        value: HExpr,
        pos: Pos,
    },
    Expr(HExpr),
    Return(Option<HExpr>, Pos),
    Break(Option<HExpr>, Pos),
    Continue(Pos),
    /// A local item (nested `fn`/`const`).
    Fn(HFn),
    Const(HConst),
}

/// A typed expression: every node carries its [`Ty`] and source [`Pos`].
pub(crate) struct HExpr {
    pub kind: HExprKind,
    pub ty: Ty,
    pub pos: Pos,
}

pub(crate) enum HExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Char(char),
    Bool(bool),
    Nil,
    /// The format primitive: render one value to `str`, with an optional format
    /// spec (`:02x`, `:.1f`, `:>20`). f-strings desugar to a `+`-concatenation of
    /// `Str` literals and `Format` nodes (Phase 2.4); the spec is honoured by the
    /// runtime in Phase 4.3.
    Format {
        value: Box<HExpr>,
        spec: Option<String>,
    },
    /// A use of a local binding, resolved to its unique id.
    Local(BindingId),
    /// A use of a global name (function, const, type, enum variant, builtin).
    Global(String),
    /// `Type::method` / `Module::item` path.
    Path(Vec<String>),

    Unary {
        op: UnOp,
        expr: Box<HExpr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<HExpr>,
        rhs: Box<HExpr>,
    },
    /// Plain assignment. Compound `+=`/`-=`/… are desugared to `x = x <op> e`
    /// (Phase 2.4), so HIR only ever sees `=`.
    Assign {
        target: Box<HExpr>,
        value: Box<HExpr>,
    },
    Cast {
        expr: Box<HExpr>,
        ty: Ty,
    },
    Call {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
    },
    MethodCall {
        recv: Box<HExpr>,
        method: String,
        args: Vec<HExpr>,
    },
    Field {
        recv: Box<HExpr>,
        name: String,
    },
    Index {
        recv: Box<HExpr>,
        index: Box<HExpr>,
    },
    Tuple(Vec<HExpr>),
    List(Vec<HExpr>),
    ListRepeat {
        value: Box<HExpr>,
        count: Box<HExpr>,
    },
    Map(Vec<(HExpr, HExpr)>),
    Set(Vec<HExpr>),
    StructLit {
        name: String,
        fields: Vec<(String, HExpr)>,
        spread: Option<Box<HExpr>>,
    },
    Block(HBlock),
    If {
        cond: Box<HExpr>,
        then: HBlock,
        els: Option<Box<HExpr>>,
    },
    Match {
        scrutinee: Box<HExpr>,
        arms: Vec<HMatchArm>,
    },
    Loop {
        body: HBlock,
    },
    While {
        cond: Box<HExpr>,
        body: HBlock,
    },
    For {
        pattern: HPattern,
        iter: Box<HExpr>,
        body: HBlock,
    },
    Range {
        start: Box<HExpr>,
        end: Box<HExpr>,
        inclusive: bool,
    },
    Closure {
        params: Vec<HParam>,
        body: Box<HExpr>,
        is_move: bool,
    },
    Await(Box<HExpr>),
    Spawn(HBlock),
    Unsafe(HBlock),
    TryCatch {
        body: HBlock,
        catches: Vec<HCatchArm>,
        finally: Option<HBlock>,
    },
}

pub(crate) struct HCatchArm {
    pub binding: Option<BindingId>,
    pub ty: Option<String>,
    pub body: HBlock,
}

pub(crate) struct HMatchArm {
    pub pattern: HPattern,
    pub guard: Option<HExpr>,
    pub body: HExpr,
}

/// A pattern with every binding site resolved to its [`BindingId`].
pub(crate) enum HPattern {
    Wildcard,
    Int(i64),
    Str(String),
    Bool(bool),
    Char(char),
    Nil,
    Binding(BindingId),
    At(BindingId, Box<HPattern>),
    Range {
        lo: i64,
        hi: i64,
        inclusive: bool,
    },
    Tuple(Vec<HPattern>),
    List {
        items: Vec<HPattern>,
        rest: Option<BindingId>,
    },
    Variant {
        path: Vec<String>,
        args: Vec<HPattern>,
    },
    Struct {
        name: String,
        fields: Vec<(String, BindingId)>,
    },
    Typed {
        binding: BindingId,
        ty: Ty,
    },
    Or(Vec<HPattern>),
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

/// Lower a checked program to HIR. Requires the [`TypeTable`] (for embedded
/// types) and the [`crate::checker::Resolutions`] (for binding ids); both come
/// from a clean front-end run, so lowering never has to handle errors.
pub(crate) fn lower(
    prog: &Program,
    types: &TypeTable,
    res: &crate::checker::Resolutions,
) -> Hir {
    let mut lo = Lower::new(prog, types, res);
    lo.lower_program(prog)
}

struct Lower<'a> {
    types: &'a TypeTable,
    res: &'a crate::checker::Resolutions,
    tyres: TyResolver,
    /// The next [`BindingId`] to hand out. Name resolution allocated ids in a
    /// fixed pre-order walk; lowering mirrors that walk exactly, so a sequential
    /// counter reproduces the same ids. The `debug_assert` in [`Self::declare`]
    /// catches any drift between the two walks.
    next_def: u32,
    /// The next *synthetic* binding id, for desugaring temporaries (the bound
    /// value in a `??`/`?.`/`?` match). Starts past every real binding so it
    /// never collides; these ids do not go through [`Self::declare`] (there is no
    /// source name to assert against).
    next_synth: u32,
    /// Generic parameter names of the item currently being lowered (so the type
    /// resolver renders `T` as [`Ty::Param`] rather than a nominal type).
    generics: HashSet<String>,
}

impl<'a> Lower<'a> {
    fn new(prog: &Program, types: &'a TypeTable, res: &'a crate::checker::Resolutions) -> Self {
        Lower {
            types,
            res,
            tyres: TyResolver::collect(prog),
            next_def: 0,
            next_synth: res.binding_count(),
            generics: HashSet::new(),
        }
    }

    /// Allocate a fresh synthetic binding id for a desugaring temporary.
    fn fresh(&mut self) -> BindingId {
        let id = BindingId(self.next_synth);
        self.next_synth += 1;
        id
    }

    // -- small HIR constructors, to keep the desugarings readable --

    fn mk(&self, kind: HExprKind, ty: Ty, pos: Pos) -> HExpr {
        HExpr { kind, ty, pos }
    }
    fn mk_nil(&self, pos: Pos) -> HExpr {
        self.mk(HExprKind::Nil, Ty::Nil, pos)
    }
    fn mk_local(&self, b: BindingId, ty: Ty, pos: Pos) -> HExpr {
        self.mk(HExprKind::Local(b), ty, pos)
    }
    /// A block that is just a `break` (the catch-all arm of a desugared `while
    /// let`). Its type is `Never` — it diverges.
    fn mk_break_block(&self, pos: Pos) -> HExpr {
        let block = HBlock {
            stmts: vec![HStmt::Break(None, pos)],
            tail: None,
            pos,
        };
        self.mk(HExprKind::Block(block), Ty::Never, pos)
    }

    /// Allocate the next binding id, mirroring name resolution's allocation
    /// order. The assertion ties the two walks together: if they ever diverge,
    /// the expected name will not match the resolver's record.
    fn declare(&mut self, name: &str) -> BindingId {
        let id = BindingId(self.next_def);
        self.next_def += 1;
        debug_assert_eq!(
            self.res.name(id),
            name,
            "hir lowering walked binding sites in a different order than name resolution"
        );
        id
    }

    fn ty_of(&self, e: &Expr) -> Ty {
        self.types.ty_of(e.id).cloned().unwrap_or(Ty::Unknown)
    }

    fn resolve_ty(&self, te: &TypeExpr) -> Ty {
        self.tyres.resolve(te, &self.generics)
    }

    fn lower_program(&mut self, prog: &Program) -> Hir {
        let mut hir = Hir {
            structs: Vec::new(),
            enums: Vec::new(),
            consts: Vec::new(),
            fns: Vec::new(),
        };
        // Walk items in source order: name resolution numbered binding sites in
        // exactly this order, so the counter in `declare` stays aligned.
        for item in &prog.items {
            match item {
                Item::Fn(f) => {
                    let hf = self.lower_fn(f, None);
                    hir.fns.push(hf);
                }
                Item::Impl(b) => {
                    for m in &b.methods {
                        let hf = self.lower_fn(m, Some(b.ty.as_str()));
                        hir.fns.push(hf);
                    }
                }
                Item::Const(c) => {
                    let value = self.lower_expr(&c.value);
                    let ty = match &c.ty {
                        Some(te) => self.resolve_ty(te),
                        None => value.ty.clone(),
                    };
                    hir.consts.push(HConst {
                        name: c.name.clone(),
                        ty,
                        value,
                    });
                }
                Item::Struct(s) => hir.structs.push(self.lower_struct(s)),
                Item::Enum(e) => hir.enums.push(self.lower_enum(e)),
                Item::Use(_) | Item::TypeAlias { .. } | Item::Interface(_) => {}
            }
        }
        hir
    }

    fn lower_struct(&mut self, s: &StructDecl) -> HStruct {
        self.generics = s.generics.iter().cloned().collect();
        let fields = s
            .fields
            .iter()
            .map(|(n, te)| (n.clone(), self.resolve_ty(te)))
            .collect();
        self.generics.clear();
        HStruct {
            name: s.name.clone(),
            fields,
        }
    }

    fn lower_enum(&mut self, e: &EnumDecl) -> HEnum {
        self.generics = e.generics.iter().cloned().collect();
        let variants = e
            .variants
            .iter()
            .map(|v| HVariant {
                name: v.name.clone(),
                kind: match &v.kind {
                    VariantKind::Unit => HVariantKind::Unit,
                    VariantKind::Tuple(ts) => {
                        HVariantKind::Tuple(ts.iter().map(|t| self.resolve_ty(t)).collect())
                    }
                    VariantKind::Struct(fs) => HVariantKind::Struct(
                        fs.iter().map(|(n, t)| (n.clone(), self.resolve_ty(t))).collect(),
                    ),
                },
            })
            .collect();
        self.generics.clear();
        HEnum {
            name: e.name.clone(),
            variants,
        }
    }

    fn lower_fn(&mut self, f: &FnDecl, owner: Option<&str>) -> HFn {
        // Save the enclosing item's generics so a nested `fn` restores them.
        let saved = std::mem::take(&mut self.generics);
        self.generics = f.generics.iter().map(|(g, _)| g.clone()).collect();
        // Name resolution declares generics, then params, then the variadic —
        // mirror that order. Generics get ids but are not value bindings here.
        for (g, _) in &f.generics {
            self.declare(g);
        }
        let mut params = Vec::new();
        for p in &f.params {
            params.push(self.lower_param(p, owner));
        }
        let variadic = f.variadic.as_ref().map(|p| self.lower_param(p, owner));
        let ret = f.ret.as_ref().map(|te| self.resolve_ty(te)).unwrap_or(Ty::Unit);
        let body = self.lower_block(&f.body);
        self.generics = saved;
        HFn {
            name: f.name.clone(),
            owner: owner.map(|s| s.to_string()),
            self_kind: f.self_kind,
            params,
            variadic,
            ret,
            body,
            is_async: f.is_async,
            pos: f.pos,
        }
    }

    fn lower_param(&mut self, p: &Param, owner: Option<&str>) -> HParam {
        let name = if p.is_self { "self" } else { p.name.as_str() };
        let binding = self.declare(name);
        let ty = match &p.ty {
            Some(te) => self.resolve_ty(te),
            None if p.is_self => match owner {
                // An unannotated `self`: its type is the owning nominal type.
                Some(o) => self.resolve_ty(&TypeExpr::Named {
                    name: o.to_string(),
                    args: Vec::new(),
                }),
                None => Ty::Unknown,
            },
            None => Ty::Unknown,
        };
        HParam {
            binding,
            name: name.to_string(),
            ty,
        }
    }

    fn lower_block(&mut self, b: &Block) -> HBlock {
        let stmts = b.stmts.iter().map(|s| self.lower_stmt(s)).collect();
        let tail = b.tail.as_ref().map(|t| Box::new(self.lower_expr(t)));
        HBlock {
            stmts,
            tail,
            pos: b.pos,
        }
    }

    fn lower_stmt(&mut self, s: &Stmt) -> HStmt {
        match s {
            Stmt::Let {
                pattern,
                mutable,
                ty,
                value,
                pos,
            } => {
                // Resolution walks the value before binding the pattern.
                let value = self.lower_expr(value);
                let pattern = self.lower_pattern(pattern);
                let bty = match ty {
                    Some(te) => self.resolve_ty(te),
                    None => value.ty.clone(),
                };
                HStmt::Let {
                    pattern,
                    mutable: *mutable,
                    ty: bty,
                    value,
                    pos: *pos,
                }
            }
            Stmt::Expr(e) => HStmt::Expr(self.lower_expr(e)),
            Stmt::Return(e, pos) => HStmt::Return(e.as_ref().map(|e| self.lower_expr(e)), *pos),
            Stmt::Break(e, pos) => HStmt::Break(e.as_ref().map(|e| self.lower_expr(e)), *pos),
            Stmt::Continue(pos) => HStmt::Continue(*pos),
            Stmt::Item(item) => match item {
                Item::Fn(f) => {
                    let id = self.declare(&f.name);
                    let _ = id; // the name binds a local; its id is recorded in HFn by name
                    HStmt::Fn(self.lower_fn(f, None))
                }
                Item::Const(c) => {
                    let value = self.lower_expr(&c.value);
                    self.declare(&c.name);
                    let ty = match &c.ty {
                        Some(te) => self.resolve_ty(te),
                        None => value.ty.clone(),
                    };
                    HStmt::Const(HConst {
                        name: c.name.clone(),
                        ty,
                        value,
                    })
                }
                // Other nested items carry no bindings/expressions; resolution
                // ignores them, so they leave the id counter untouched.
                _ => HStmt::Continue(Pos { line: 0, col: 0 }),
            },
        }
    }

    fn lower_expr(&mut self, e: &Expr) -> HExpr {
        // The Phase 2.4 desugarings produce a whole replacement subtree, so they
        // are handled here (each returns a full `HExpr`); everything else lowers
        // 1:1 via `lower_kind`.
        match &e.kind {
            ExprKind::FStr(parts) => return self.desugar_fstring(parts, e.pos),
            ExprKind::Coalesce { lhs, rhs } => return self.desugar_coalesce(e, lhs, rhs),
            ExprKind::Try(inner) => return self.desugar_try(e, inner),
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => return self.desugar_while_let(e, pattern, expr, body),
            ExprKind::Assign {
                target,
                op: Some(op),
                value,
            } => return self.desugar_compound(e, target, *op, value),
            ExprKind::MethodCall {
                recv,
                optional: true,
                method,
                args,
                ..
            } => return self.desugar_optional_method(e, recv, method, args),
            ExprKind::Field {
                recv,
                optional: true,
                name,
            } => return self.desugar_optional_field(e, recv, name),
            _ => {}
        }
        let ty = self.ty_of(e);
        let kind = self.lower_kind(e);
        HExpr {
            kind,
            ty,
            pos: e.pos,
        }
    }

    fn lower_kind(&mut self, e: &Expr) -> HExprKind {
        match &e.kind {
            ExprKind::Int(v) => HExprKind::Int(*v),
            ExprKind::Float(v) => HExprKind::Float(*v),
            ExprKind::Str(s) => HExprKind::Str(s.clone()),
            ExprKind::Char(c) => HExprKind::Char(*c),
            ExprKind::Bool(b) => HExprKind::Bool(*b),
            ExprKind::Nil => HExprKind::Nil,
            ExprKind::Path(p) => HExprKind::Path(p.clone()),
            ExprKind::Ident(name) => match self.res.binding_of(e.id) {
                Some(b) => HExprKind::Local(b),
                None => HExprKind::Global(name.clone()),
            },
            ExprKind::SelfExpr => match self.res.binding_of(e.id) {
                Some(b) => HExprKind::Local(b),
                None => HExprKind::Global("self".to_string()),
            },
            // Desugared in `lower_expr` before reaching here.
            ExprKind::FStr(_)
            | ExprKind::Coalesce { .. }
            | ExprKind::Try(_)
            | ExprKind::WhileLet { .. } => {
                unreachable!("sugar is desugared in lower_expr")
            }
            ExprKind::Unary { op, expr } => HExprKind::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(expr)),
            },
            ExprKind::Binary { op, lhs, rhs } => HExprKind::Binary {
                op: *op,
                lhs: Box::new(self.lower_expr(lhs)),
                rhs: Box::new(self.lower_expr(rhs)),
            },
            // Compound `+=` is desugared in `lower_expr`; only plain `=` reaches here.
            ExprKind::Assign { target, value, .. } => HExprKind::Assign {
                target: Box::new(self.lower_expr(target)),
                value: Box::new(self.lower_expr(value)),
            },
            ExprKind::Cast { expr, .. } => HExprKind::Cast {
                expr: Box::new(self.lower_expr(expr)),
                // The node's recorded type is the cast's target type.
                ty: self.ty_of(e),
            },
            ExprKind::Call { callee, args } => HExprKind::Call {
                callee: Box::new(self.lower_expr(callee)),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
            },
            // `?.` is desugared in `lower_expr`; only plain calls/fields reach here.
            ExprKind::MethodCall {
                recv, method, args, ..
            } => HExprKind::MethodCall {
                recv: Box::new(self.lower_expr(recv)),
                method: method.clone(),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
            },
            ExprKind::Field { recv, name, .. } => HExprKind::Field {
                recv: Box::new(self.lower_expr(recv)),
                name: name.clone(),
            },
            ExprKind::Index { recv, index } => HExprKind::Index {
                recv: Box::new(self.lower_expr(recv)),
                index: Box::new(self.lower_expr(index)),
            },
            ExprKind::Tuple(xs) => HExprKind::Tuple(xs.iter().map(|x| self.lower_expr(x)).collect()),
            ExprKind::List(xs) => HExprKind::List(xs.iter().map(|x| self.lower_expr(x)).collect()),
            ExprKind::Set(xs) => HExprKind::Set(xs.iter().map(|x| self.lower_expr(x)).collect()),
            ExprKind::ListRepeat { value, count } => HExprKind::ListRepeat {
                value: Box::new(self.lower_expr(value)),
                count: Box::new(self.lower_expr(count)),
            },
            ExprKind::Map(pairs) => HExprKind::Map(
                pairs
                    .iter()
                    .map(|(k, v)| (self.lower_expr(k), self.lower_expr(v)))
                    .collect(),
            ),
            ExprKind::StructLit {
                name,
                fields,
                spread,
            } => HExprKind::StructLit {
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(n, v)| (n.clone(), self.lower_expr(v)))
                    .collect(),
                spread: spread.as_ref().map(|s| Box::new(self.lower_expr(s))),
            },
            ExprKind::Block(b) => HExprKind::Block(self.lower_block(b)),
            ExprKind::If { cond, then, els } => HExprKind::If {
                cond: Box::new(self.lower_expr(cond)),
                then: self.lower_block(then),
                els: els.as_ref().map(|e| Box::new(self.lower_expr(e))),
            },
            ExprKind::Match { scrutinee, arms } => HExprKind::Match {
                scrutinee: Box::new(self.lower_expr(scrutinee)),
                arms: arms
                    .iter()
                    .map(|arm| {
                        // Resolution binds the pattern, then walks guard, then body.
                        let pattern = self.lower_pattern(&arm.pattern);
                        let guard = arm.guard.as_ref().map(|g| self.lower_expr(g));
                        let body = self.lower_expr(&arm.body);
                        HMatchArm {
                            pattern,
                            guard,
                            body,
                        }
                    })
                    .collect(),
            },
            ExprKind::Loop { body } => HExprKind::Loop {
                body: self.lower_block(body),
            },
            ExprKind::While { cond, body } => HExprKind::While {
                cond: Box::new(self.lower_expr(cond)),
                body: self.lower_block(body),
            },
            ExprKind::For {
                pattern,
                iter,
                body,
            } => {
                let iter = Box::new(self.lower_expr(iter));
                let pattern = self.lower_pattern(pattern);
                HExprKind::For {
                    pattern,
                    iter,
                    body: self.lower_block(body),
                }
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => HExprKind::Range {
                start: Box::new(self.lower_expr(start)),
                end: Box::new(self.lower_expr(end)),
                inclusive: *inclusive,
            },
            ExprKind::Closure {
                params,
                body,
                is_move,
            } => {
                let params = params
                    .iter()
                    .map(|p| {
                        let binding = self.declare(&p.name);
                        let ty = p
                            .ty
                            .as_ref()
                            .map(|te| self.resolve_ty(te))
                            .unwrap_or(Ty::Unknown);
                        HParam {
                            binding,
                            name: p.name.clone(),
                            ty,
                        }
                    })
                    .collect();
                HExprKind::Closure {
                    params,
                    body: Box::new(self.lower_expr(body)),
                    is_move: *is_move,
                }
            }
            ExprKind::Await(e) => HExprKind::Await(Box::new(self.lower_expr(e))),
            ExprKind::Spawn(b) => HExprKind::Spawn(self.lower_block(b)),
            ExprKind::Unsafe(b) => HExprKind::Unsafe(self.lower_block(b)),
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                let body = self.lower_block(body);
                let catches = catches
                    .iter()
                    .map(|c| {
                        let binding = c.binding.as_ref().map(|b| self.declare(b));
                        HCatchArm {
                            binding,
                            ty: c.ty.clone(),
                            body: self.lower_block(&c.body),
                        }
                    })
                    .collect();
                let finally = finally.as_ref().map(|f| self.lower_block(f));
                HExprKind::TryCatch {
                    body,
                    catches,
                    finally,
                }
            }
        }
    }

    fn lower_pattern(&mut self, p: &Pattern) -> HPattern {
        match p {
            Pattern::Wildcard => HPattern::Wildcard,
            Pattern::Int(v) => HPattern::Int(*v),
            Pattern::Str(s) => HPattern::Str(s.clone()),
            Pattern::Bool(b) => HPattern::Bool(*b),
            Pattern::Char(c) => HPattern::Char(*c),
            Pattern::Nil => HPattern::Nil,
            Pattern::Binding(n) => HPattern::Binding(self.declare(n)),
            Pattern::At(n, sub) => {
                let id = self.declare(n);
                HPattern::At(id, Box::new(self.lower_pattern(sub)))
            }
            Pattern::Range { lo, hi, inclusive } => HPattern::Range {
                lo: *lo,
                hi: *hi,
                inclusive: *inclusive,
            },
            Pattern::Tuple(ps) => HPattern::Tuple(ps.iter().map(|p| self.lower_pattern(p)).collect()),
            Pattern::Or(ps) => HPattern::Or(ps.iter().map(|p| self.lower_pattern(p)).collect()),
            Pattern::List { items, rest } => {
                let items = items.iter().map(|p| self.lower_pattern(p)).collect();
                // Resolution only declares a non-empty rest name.
                let rest = rest.as_ref().filter(|r| !r.is_empty()).map(|r| self.declare(r));
                HPattern::List { items, rest }
            }
            Pattern::Variant { path, args } => HPattern::Variant {
                path: path.clone(),
                args: args.iter().map(|p| self.lower_pattern(p)).collect(),
            },
            Pattern::Struct { name, fields } => HPattern::Struct {
                name: name.clone(),
                fields: fields.iter().map(|f| (f.clone(), self.declare(f))).collect(),
            },
            Pattern::Typed { binding, ty } => {
                let id = self.declare(binding);
                HPattern::Typed {
                    binding: id,
                    ty: self.resolve_ty(ty),
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2.4 desugarings (each returns a full replacement `HExpr`)
    // -----------------------------------------------------------------------

    /// `f"a={x:spec}"` → a `+`-fold of `Str` literals and `Format` primitives.
    /// A literal-only or empty f-string collapses to a single `Str`.
    fn desugar_fstring(&mut self, parts: &[FStrPart], pos: Pos) -> HExpr {
        let mut segs: Vec<HExpr> = Vec::new();
        for part in parts {
            match part {
                FStrPart::Lit(s) => segs.push(self.mk(HExprKind::Str(s.clone()), Ty::Str, pos)),
                FStrPart::Expr { expr, spec } => {
                    let value = Box::new(self.lower_expr(expr));
                    segs.push(self.mk(
                        HExprKind::Format {
                            value,
                            spec: spec.clone(),
                        },
                        Ty::Str,
                        pos,
                    ));
                }
            }
        }
        let mut iter = segs.into_iter();
        let mut acc = iter
            .next()
            .unwrap_or_else(|| self.mk(HExprKind::Str(String::new()), Ty::Str, pos));
        for seg in iter {
            acc = self.mk(
                HExprKind::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(acc),
                    rhs: Box::new(seg),
                },
                Ty::Str,
                pos,
            );
        }
        acc
    }

    /// `a ?? b` → `match a { nil => b, t => t }` (`t` fresh). Mirrors the
    /// interpreter: the left value when non-`nil`, else the right (short-circuit).
    fn desugar_coalesce(&mut self, e: &Expr, lhs: &Expr, rhs: &Expr) -> HExpr {
        let node_ty = self.ty_of(e);
        let scrut = self.lower_expr(lhs);
        let lty = scrut.ty.clone();
        let rhs = self.lower_expr(rhs);
        let t = self.fresh();
        let arms = vec![
            HMatchArm {
                pattern: HPattern::Nil,
                guard: None,
                body: rhs,
            },
            HMatchArm {
                pattern: HPattern::Binding(t),
                guard: None,
                body: self.mk_local(t, lty, e.pos),
            },
        ];
        self.mk(
            HExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            node_ty,
            e.pos,
        )
    }

    /// `a?.name` → `match a { nil => nil, t => t.name }` (`t` fresh).
    fn desugar_optional_field(&mut self, e: &Expr, recv: &Expr, name: &str) -> HExpr {
        let node_ty = self.ty_of(e); // `T | nil`
        let scrut = self.lower_expr(recv);
        let rty = scrut.ty.clone();
        let t = self.fresh();
        let access = self.mk(
            HExprKind::Field {
                recv: Box::new(self.mk_local(t, rty, e.pos)),
                name: name.to_string(),
            },
            node_ty.strip_nil(),
            e.pos,
        );
        self.mk_optional_match(scrut, t, access, node_ty, e.pos)
    }

    /// `a?.m(args)` → `match a { nil => nil, t => t.m(args) }` (`t` fresh). The
    /// args sit in the non-`nil` arm, so they are evaluated only when reached.
    fn desugar_optional_method(
        &mut self,
        e: &Expr,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> HExpr {
        let node_ty = self.ty_of(e);
        let scrut = self.lower_expr(recv);
        let rty = scrut.ty.clone();
        let t = self.fresh();
        let largs = args.iter().map(|a| self.lower_expr(a)).collect();
        let access = self.mk(
            HExprKind::MethodCall {
                recv: Box::new(self.mk_local(t, rty, e.pos)),
                method: method.to_string(),
                args: largs,
            },
            node_ty.strip_nil(),
            e.pos,
        );
        self.mk_optional_match(scrut, t, access, node_ty, e.pos)
    }

    /// Shared shape for `?.`: `match scrut { nil => nil, t => access }`.
    fn mk_optional_match(
        &mut self,
        scrut: HExpr,
        t: BindingId,
        access: HExpr,
        node_ty: Ty,
        pos: Pos,
    ) -> HExpr {
        let arms = vec![
            HMatchArm {
                pattern: HPattern::Nil,
                guard: None,
                body: self.mk_nil(pos),
            },
            HMatchArm {
                pattern: HPattern::Binding(t),
                guard: None,
                body: access,
            },
        ];
        self.mk(
            HExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            node_ty,
            pos,
        )
    }

    /// `x op= e` → `x = x op e`. Per the plan the place is re-evaluated, which
    /// double-evaluates a non-trivial place's sub-expressions (e.g. the index in
    /// `a[i] += 1`); assignment targets are simple lvalues, and the precise
    /// "evaluate the place once" semantics is a MIR concern (explicit places).
    fn desugar_compound(&mut self, e: &Expr, target: &Expr, op: BinOp, value: &Expr) -> HExpr {
        let lhs_place = self.lower_expr(target);
        let lhs_operand = self.lower_expr(target);
        let operand_ty = lhs_operand.ty.clone();
        let rhs = self.lower_expr(value);
        let combined = self.mk(
            HExprKind::Binary {
                op,
                lhs: Box::new(lhs_operand),
                rhs: Box::new(rhs),
            },
            operand_ty,
            e.pos,
        );
        self.mk(
            HExprKind::Assign {
                target: Box::new(lhs_place),
                value: Box::new(combined),
            },
            self.ty_of(e),
            e.pos,
        )
    }

    /// `e?` → a `match` that unwraps and early-returns, picked by `e`'s type:
    /// `Result` → `{ Ok(v) => v, Err(x) => return Err(x) }`; otherwise (Option /
    /// bare optional) → `{ Some(v) => v, None => return nil }`. The `nil` early
    /// return matches the interpreter oracle (it returns `nil`, not `None`).
    fn desugar_try(&mut self, e: &Expr, inner: &Expr) -> HExpr {
        let node_ty = self.ty_of(e); // the unwrapped payload type
        let scrut = self.lower_expr(inner);
        let inner_ty = scrut.ty.clone();
        let is_result = matches!(&inner_ty, Ty::Enum(n, _) if n == "Result");
        let v = self.fresh();
        let arms = if is_result {
            let x = self.fresh();
            let err_call = self.mk(
                HExprKind::Call {
                    callee: Box::new(self.mk(HExprKind::Global("Err".into()), Ty::Unknown, e.pos)),
                    args: vec![self.mk_local(x, Ty::Unknown, e.pos)],
                },
                inner_ty.clone(),
                e.pos,
            );
            let err_body = self.mk_return_block(err_call, e.pos);
            vec![
                HMatchArm {
                    pattern: HPattern::Variant {
                        path: vec!["Ok".into()],
                        args: vec![HPattern::Binding(v)],
                    },
                    guard: None,
                    body: self.mk_local(v, node_ty.clone(), e.pos),
                },
                HMatchArm {
                    pattern: HPattern::Variant {
                        path: vec!["Err".into()],
                        args: vec![HPattern::Binding(x)],
                    },
                    guard: None,
                    body: err_body,
                },
            ]
        } else {
            let none_ret = self.mk_nil(e.pos);
            let none_body = self.mk_return_block(none_ret, e.pos);
            vec![
                HMatchArm {
                    pattern: HPattern::Variant {
                        path: vec!["Some".into()],
                        args: vec![HPattern::Binding(v)],
                    },
                    guard: None,
                    body: self.mk_local(v, node_ty.clone(), e.pos),
                },
                HMatchArm {
                    pattern: HPattern::Variant {
                        path: vec!["None".into()],
                        args: vec![],
                    },
                    guard: None,
                    body: none_body,
                },
            ]
        };
        self.mk(
            HExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            node_ty,
            e.pos,
        )
    }

    /// `while let P = e { body }` → `loop { match e { P => body, _ => break } }`.
    fn desugar_while_let(
        &mut self,
        e: &Expr,
        pattern: &Pattern,
        expr: &Expr,
        body: &Block,
    ) -> HExpr {
        // Mirror resolution order: scrutinee, then bind the pattern, then body.
        let scrut = self.lower_expr(expr);
        let pat = self.lower_pattern(pattern);
        let body_block = self.lower_block(body);
        let body_expr = self.mk(HExprKind::Block(body_block), Ty::Unit, e.pos);
        let arms = vec![
            HMatchArm {
                pattern: pat,
                guard: None,
                body: body_expr,
            },
            HMatchArm {
                pattern: HPattern::Wildcard,
                guard: None,
                body: self.mk_break_block(e.pos),
            },
        ];
        let match_expr = self.mk(
            HExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            Ty::Unit,
            e.pos,
        );
        let loop_block = HBlock {
            stmts: Vec::new(),
            tail: Some(Box::new(match_expr)),
            pos: e.pos,
        };
        self.mk(HExprKind::Loop { body: loop_block }, self.ty_of(e), e.pos)
    }

    /// A block whose only statement is `return <val>`; type `Never`.
    fn mk_return_block(&self, val: HExpr, pos: Pos) -> HExpr {
        let block = HBlock {
            stmts: vec![HStmt::Return(Some(val), pos)],
            tail: None,
            pos,
        };
        self.mk(HExprKind::Block(block), Ty::Never, pos)
    }
}

// ---------------------------------------------------------------------------
// TypeExpr → Ty resolution (for binding sites, which carry no NodeId)
// ---------------------------------------------------------------------------

/// Resolves surface [`TypeExpr`]s to semantic [`Ty`]s for the places the
/// [`TypeTable`] cannot reach — parameters, fields, and return types have no
/// [`NodeId`]. It mirrors the type checker's `resolve_in` over the program's
/// nominal declarations, so HIR sees the same `Ty` everywhere.
struct TyResolver {
    structs: HashSet<String>,
    enums: HashSet<String>,
    aliases: HashMap<String, TypeExpr>,
}

impl TyResolver {
    fn collect(prog: &Program) -> Self {
        let mut structs = HashSet::new();
        let mut enums = HashSet::new();
        let mut aliases = HashMap::new();
        for item in &prog.items {
            match item {
                Item::Struct(s) => {
                    structs.insert(s.name.clone());
                }
                Item::Enum(e) => {
                    enums.insert(e.name.clone());
                }
                Item::TypeAlias { name, ty } => {
                    aliases.insert(name.clone(), ty.clone());
                }
                _ => {}
            }
        }
        TyResolver {
            structs,
            enums,
            aliases,
        }
    }

    fn resolve(&self, t: &TypeExpr, generics: &HashSet<String>) -> Ty {
        match t {
            TypeExpr::Named { name, args } => {
                let rargs: Vec<Ty> = args.iter().map(|a| self.resolve(a, generics)).collect();
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
                            self.resolve(&alias.clone(), generics)
                        } else if self.structs.contains(name) {
                            Ty::Struct(name.clone(), rargs)
                        } else if self.enums.contains(name) {
                            Ty::Enum(name.clone(), rargs)
                        } else {
                            Ty::Unknown
                        }
                    }
                }
            }
            TypeExpr::Ref { inner, .. } => Ty::Ref(Box::new(self.resolve(inner, generics))),
            TypeExpr::Ptr { inner, .. } => Ty::Ptr(Box::new(self.resolve(inner, generics))),
            TypeExpr::Array { inner, size } => {
                Ty::Array(Box::new(self.resolve(inner, generics)), size.map(|s| s as usize))
            }
            TypeExpr::Slice(inner) => Ty::Slice(Box::new(self.resolve(inner, generics))),
            TypeExpr::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve(t, generics)).collect()),
            TypeExpr::Union(ts) => {
                Ty::Union(ts.iter().map(|t| self.resolve(t, generics)).collect())
            }
            TypeExpr::Fn { params, ret } => Ty::Fn(
                params.iter().map(|t| self.resolve(t, generics)).collect(),
                Box::new(self.resolve(ret, generics)),
            ),
            TypeExpr::Async(inner) => Ty::Future(Box::new(self.resolve(inner, generics))),
            TypeExpr::Unit => Ty::Unit,
            TypeExpr::Never => Ty::Never,
        }
    }
}

fn arg0(args: &[Ty]) -> Ty {
    args.first().cloned().unwrap_or(Ty::Unknown)
}

fn argn(args: &[Ty], n: usize) -> Ty {
    args.get(n).cloned().unwrap_or(Ty::Unknown)
}

// ---------------------------------------------------------------------------
// Debug dump (`la3 hir`)
// ---------------------------------------------------------------------------

impl Hir {
    /// A readable, indented rendering of the HIR for the `la3 hir` command and
    /// for tests. Shows embedded types (`: ty`) and binding ids (`#n`).
    pub(crate) fn dump(&self) -> String {
        let mut p = Printer { out: String::new() };
        for s in &self.structs {
            p.line(0, &format!("struct {}", s.name));
            for (n, t) in &s.fields {
                p.line(1, &format!("{}: {}", n, display_ty(t)));
            }
        }
        for e in &self.enums {
            p.line(0, &format!("enum {}", e.name));
            for v in &e.variants {
                let detail = match &v.kind {
                    HVariantKind::Unit => String::new(),
                    HVariantKind::Tuple(ts) => {
                        let inner: Vec<String> = ts.iter().map(display_ty).collect();
                        format!("({})", inner.join(", "))
                    }
                    HVariantKind::Struct(fs) => {
                        let inner: Vec<String> =
                            fs.iter().map(|(n, t)| format!("{}: {}", n, display_ty(t))).collect();
                        format!(" {{ {} }}", inner.join(", "))
                    }
                };
                p.line(1, &format!("{}{}", v.name, detail));
            }
        }
        for c in &self.consts {
            p.line(0, &format!("const {}: {}", c.name, display_ty(&c.ty)));
            p.expr(1, &c.value);
        }
        for f in &self.fns {
            let recv = match f.owner {
                Some(ref o) => format!("{}::", o),
                None => String::new(),
            };
            let params: Vec<String> = f
                .params
                .iter()
                .map(|pa| format!("{}#{}: {}", pa.name, pa.binding.0, display_ty(&pa.ty)))
                .collect();
            p.line(
                0,
                &format!(
                    "fn {}{}({}) -> {}",
                    recv,
                    f.name,
                    params.join(", "),
                    display_ty(&f.ret)
                ),
            );
            p.block(1, &f.body);
        }
        p.out
    }
}

struct Printer {
    out: String,
}

impl Printer {
    fn line(&mut self, indent: usize, s: &str) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn block(&mut self, indent: usize, b: &HBlock) {
        for s in &b.stmts {
            self.stmt(indent, s);
        }
        if let Some(t) = &b.tail {
            self.line(indent, "tail:");
            self.expr(indent + 1, t);
        }
    }

    fn stmt(&mut self, indent: usize, s: &HStmt) {
        match s {
            HStmt::Let {
                pattern,
                mutable,
                ty,
                value,
                ..
            } => {
                let m = if *mutable { "mut " } else { "" };
                self.line(
                    indent,
                    &format!("let {}{} : {}", m, pat_str(pattern), display_ty(ty)),
                );
                self.expr(indent + 1, value);
            }
            HStmt::Expr(e) => self.expr(indent, e),
            HStmt::Return(e, _) => {
                self.line(indent, "return");
                if let Some(e) = e {
                    self.expr(indent + 1, e);
                }
            }
            HStmt::Break(e, _) => {
                self.line(indent, "break");
                if let Some(e) = e {
                    self.expr(indent + 1, e);
                }
            }
            HStmt::Continue(_) => self.line(indent, "continue"),
            HStmt::Fn(f) => {
                self.line(indent, &format!("fn {} (local)", f.name));
                self.block(indent + 1, &f.body);
            }
            HStmt::Const(c) => {
                self.line(indent, &format!("const {}: {}", c.name, display_ty(&c.ty)));
                self.expr(indent + 1, &c.value);
            }
        }
    }

    fn expr(&mut self, indent: usize, e: &HExpr) {
        let head = match &e.kind {
            HExprKind::Int(v) => format!("Int({})", v),
            HExprKind::Float(v) => format!("Float({})", v),
            HExprKind::Str(s) => format!("Str({:?})", s),
            HExprKind::Char(c) => format!("Char({:?})", c),
            HExprKind::Bool(b) => format!("Bool({})", b),
            HExprKind::Nil => "Nil".to_string(),
            HExprKind::Local(b) => format!("Local(#{})", b.0),
            HExprKind::Global(n) => format!("Global({})", n),
            HExprKind::Path(p) => format!("Path({})", p.join("::")),
            HExprKind::Unary { op, .. } => format!("Unary({:?})", op),
            HExprKind::Binary { op, .. } => format!("Binary({:?})", op),
            HExprKind::Format { spec, .. } => match spec {
                Some(s) => format!("Format(:{})", s),
                None => "Format".to_string(),
            },
            HExprKind::Assign { .. } => "Assign".to_string(),
            HExprKind::Cast { ty, .. } => format!("Cast({})", display_ty(ty)),
            HExprKind::Call { .. } => "Call".to_string(),
            HExprKind::MethodCall { method, .. } => format!("MethodCall({})", method),
            HExprKind::Field { name, .. } => format!("Field({})", name),
            HExprKind::Index { .. } => "Index".to_string(),
            HExprKind::Tuple(_) => "Tuple".to_string(),
            HExprKind::List(_) => "List".to_string(),
            HExprKind::ListRepeat { .. } => "ListRepeat".to_string(),
            HExprKind::Map(_) => "Map".to_string(),
            HExprKind::Set(_) => "Set".to_string(),
            HExprKind::StructLit { name, .. } => format!("StructLit({})", name),
            HExprKind::Block(_) => "Block".to_string(),
            HExprKind::If { .. } => "If".to_string(),
            HExprKind::Match { .. } => "Match".to_string(),
            HExprKind::Loop { .. } => "Loop".to_string(),
            HExprKind::While { .. } => "While".to_string(),
            HExprKind::For { .. } => "For".to_string(),
            HExprKind::Range { inclusive, .. } => {
                format!("Range(inclusive={})", inclusive)
            }
            HExprKind::Closure { is_move, .. } => format!("Closure(move={})", is_move),
            HExprKind::Await(_) => "Await".to_string(),
            HExprKind::Spawn(_) => "Spawn".to_string(),
            HExprKind::Unsafe(_) => "Unsafe".to_string(),
            HExprKind::TryCatch { .. } => "TryCatch".to_string(),
        };
        self.line(indent, &format!("{} : {}", head, display_ty(&e.ty)));
        self.children(indent + 1, e);
    }

    fn children(&mut self, indent: usize, e: &HExpr) {
        match &e.kind {
            HExprKind::Unary { expr, .. }
            | HExprKind::Cast { expr, .. }
            | HExprKind::Format { value: expr, .. } => self.expr(indent, expr),
            HExprKind::Binary { lhs, rhs, .. } => {
                self.expr(indent, lhs);
                self.expr(indent, rhs);
            }
            HExprKind::Assign { target, value, .. } => {
                self.expr(indent, target);
                self.expr(indent, value);
            }
            HExprKind::Call { callee, args } => {
                self.expr(indent, callee);
                for a in args {
                    self.expr(indent, a);
                }
            }
            HExprKind::MethodCall { recv, args, .. } => {
                self.expr(indent, recv);
                for a in args {
                    self.expr(indent, a);
                }
            }
            HExprKind::Field { recv, .. } => self.expr(indent, recv),
            HExprKind::Index { recv, index } => {
                self.expr(indent, recv);
                self.expr(indent, index);
            }
            HExprKind::Tuple(xs) | HExprKind::List(xs) | HExprKind::Set(xs) => {
                for x in xs {
                    self.expr(indent, x);
                }
            }
            HExprKind::ListRepeat { value, count } => {
                self.expr(indent, value);
                self.expr(indent, count);
            }
            HExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.expr(indent, k);
                    self.expr(indent, v);
                }
            }
            HExprKind::StructLit { fields, spread, .. } => {
                for (n, v) in fields {
                    self.line(indent, &format!(".{}", n));
                    self.expr(indent + 1, v);
                }
                if let Some(s) = spread {
                    self.line(indent, "..spread");
                    self.expr(indent + 1, s);
                }
            }
            HExprKind::Block(b) => self.block(indent, b),
            HExprKind::If { cond, then, els } => {
                self.expr(indent, cond);
                self.line(indent, "then:");
                self.block(indent + 1, then);
                if let Some(e) = els {
                    self.line(indent, "else:");
                    self.expr(indent + 1, e);
                }
            }
            HExprKind::Match { scrutinee, arms } => {
                self.expr(indent, scrutinee);
                for arm in arms {
                    self.line(indent, &format!("arm {}", pat_str(&arm.pattern)));
                    if let Some(g) = &arm.guard {
                        self.line(indent + 1, "guard:");
                        self.expr(indent + 2, g);
                    }
                    self.expr(indent + 1, &arm.body);
                }
            }
            HExprKind::Loop { body } | HExprKind::Spawn(body) | HExprKind::Unsafe(body) => {
                self.block(indent, body)
            }
            HExprKind::While { cond, body } => {
                self.expr(indent, cond);
                self.block(indent, body);
            }
            HExprKind::For {
                pattern,
                iter,
                body,
            } => {
                self.line(indent, &format!("pat {}", pat_str(pattern)));
                self.expr(indent, iter);
                self.block(indent, body);
            }
            HExprKind::Range { start, end, .. } => {
                self.expr(indent, start);
                self.expr(indent, end);
            }
            HExprKind::Closure { params, body, .. } => {
                let ps: Vec<String> = params
                    .iter()
                    .map(|pa| format!("{}#{}", pa.name, pa.binding.0))
                    .collect();
                self.line(indent, &format!("params({})", ps.join(", ")));
                self.expr(indent, body);
            }
            HExprKind::Await(inner) => self.expr(indent, inner),
            HExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                self.block(indent, body);
                for c in catches {
                    let b = c.binding.map(|b| format!("#{}", b.0)).unwrap_or_default();
                    self.line(indent, &format!("catch {}", b));
                    self.block(indent + 1, &c.body);
                }
                if let Some(f) = finally {
                    self.line(indent, "finally:");
                    self.block(indent + 1, f);
                }
            }
            // Leaves: nothing to descend into.
            _ => {}
        }
    }
}

fn pat_str(p: &HPattern) -> String {
    match p {
        HPattern::Wildcard => "_".to_string(),
        HPattern::Int(v) => v.to_string(),
        HPattern::Str(s) => format!("{:?}", s),
        HPattern::Bool(b) => b.to_string(),
        HPattern::Char(c) => format!("{:?}", c),
        HPattern::Nil => "nil".to_string(),
        HPattern::Binding(b) => format!("#{}", b.0),
        HPattern::At(b, sub) => format!("#{} @ {}", b.0, pat_str(sub)),
        HPattern::Range { lo, hi, inclusive } => {
            format!("{}..{}{}", lo, if *inclusive { "=" } else { "" }, hi)
        }
        HPattern::Tuple(ps) => {
            let inner: Vec<String> = ps.iter().map(pat_str).collect();
            format!("({})", inner.join(", "))
        }
        HPattern::List { items, rest } => {
            let mut inner: Vec<String> = items.iter().map(pat_str).collect();
            if let Some(r) = rest {
                inner.push(format!("..#{}", r.0));
            }
            format!("[{}]", inner.join(", "))
        }
        HPattern::Variant { path, args } => {
            let a: Vec<String> = args.iter().map(pat_str).collect();
            if a.is_empty() {
                path.join(".")
            } else {
                format!("{}({})", path.join("."), a.join(", "))
            }
        }
        HPattern::Struct { name, fields } => {
            let fs: Vec<String> = fields.iter().map(|(n, b)| format!("{} #{}", n, b.0)).collect();
            format!("{} {{ {} }}", name, fs.join(", "))
        }
        HPattern::Typed { binding, ty } => format!("#{}: {}", binding.0, display_ty(ty)),
        HPattern::Or(ps) => {
            let inner: Vec<String> = ps.iter().map(pat_str).collect();
            inner.join(" | ")
        }
    }
}

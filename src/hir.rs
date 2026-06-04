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
//! Subpart **2.3** (this) defines the HIR and a faithful, near-1:1 lowering from
//! the AST. The *desugarings* listed in 2.4 (f-strings → `format`, `?.`/`??` →
//! `match`, `if let`/`while let` → `match`, `+=` → `x = x + e`, `e?` → early
//! return) are deliberately left for that subpart, so the sugar-carrying variants
//! below (`FStr`, `Coalesce`, compound [`HExprKind::Assign`], `WhileLet`, the
//! `optional` flags, `Try`) still exist and are lowered structurally for now.

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
    /// f-string parts (desugared to `format` in 2.4).
    FStr(Vec<HFStrPart>),
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
    /// `a ?? b` (desugared in 2.4).
    Coalesce {
        lhs: Box<HExpr>,
        rhs: Box<HExpr>,
    },
    Assign {
        target: Box<HExpr>,
        /// `None` = plain `=`; `Some` = compound (`+=`, …), desugared in 2.4.
        op: Option<BinOp>,
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
        /// `?.` short-circuit (desugared in 2.4).
        optional: bool,
        method: String,
        args: Vec<HExpr>,
    },
    Field {
        recv: Box<HExpr>,
        optional: bool,
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
    WhileLet {
        pattern: HPattern,
        expr: Box<HExpr>,
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
    Try(Box<HExpr>),
    Await(Box<HExpr>),
    Spawn(HBlock),
    Unsafe(HBlock),
    TryCatch {
        body: HBlock,
        catches: Vec<HCatchArm>,
        finally: Option<HBlock>,
    },
}

pub(crate) enum HFStrPart {
    Lit(String),
    Expr {
        expr: Box<HExpr>,
        spec: Option<String>,
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
            generics: HashSet::new(),
        }
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
            ExprKind::FStr(parts) => HExprKind::FStr(
                parts
                    .iter()
                    .map(|p| match p {
                        FStrPart::Lit(s) => HFStrPart::Lit(s.clone()),
                        FStrPart::Expr { expr, spec } => HFStrPart::Expr {
                            expr: Box::new(self.lower_expr(expr)),
                            spec: spec.clone(),
                        },
                    })
                    .collect(),
            ),
            ExprKind::Unary { op, expr } => HExprKind::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(expr)),
            },
            ExprKind::Binary { op, lhs, rhs } => HExprKind::Binary {
                op: *op,
                lhs: Box::new(self.lower_expr(lhs)),
                rhs: Box::new(self.lower_expr(rhs)),
            },
            ExprKind::Coalesce { lhs, rhs } => HExprKind::Coalesce {
                lhs: Box::new(self.lower_expr(lhs)),
                rhs: Box::new(self.lower_expr(rhs)),
            },
            ExprKind::Assign { target, op, value } => HExprKind::Assign {
                target: Box::new(self.lower_expr(target)),
                op: *op,
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
            ExprKind::MethodCall {
                recv,
                optional,
                method,
                args,
                ..
            } => HExprKind::MethodCall {
                recv: Box::new(self.lower_expr(recv)),
                optional: *optional,
                method: method.clone(),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
            },
            ExprKind::Field {
                recv,
                optional,
                name,
            } => HExprKind::Field {
                recv: Box::new(self.lower_expr(recv)),
                optional: *optional,
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
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => {
                let expr = Box::new(self.lower_expr(expr));
                let pattern = self.lower_pattern(pattern);
                HExprKind::WhileLet {
                    pattern,
                    expr,
                    body: self.lower_block(body),
                }
            }
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
            ExprKind::Try(e) => HExprKind::Try(Box::new(self.lower_expr(e))),
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
            HExprKind::Coalesce { .. } => "Coalesce".to_string(),
            HExprKind::Assign { op, .. } => format!("Assign({:?})", op),
            HExprKind::Cast { ty, .. } => format!("Cast({})", display_ty(ty)),
            HExprKind::Call { .. } => "Call".to_string(),
            HExprKind::MethodCall {
                method, optional, ..
            } => format!("MethodCall({}{})", if *optional { "?." } else { "" }, method),
            HExprKind::Field { name, optional, .. } => {
                format!("Field({}{})", if *optional { "?." } else { "" }, name)
            }
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
            HExprKind::WhileLet { .. } => "WhileLet".to_string(),
            HExprKind::For { .. } => "For".to_string(),
            HExprKind::Range { inclusive, .. } => {
                format!("Range(inclusive={})", inclusive)
            }
            HExprKind::Closure { is_move, .. } => format!("Closure(move={})", is_move),
            HExprKind::FStr(_) => "FStr".to_string(),
            HExprKind::Try(_) => "Try".to_string(),
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
            HExprKind::Unary { expr, .. } | HExprKind::Cast { expr, .. } => self.expr(indent, expr),
            HExprKind::Binary { lhs, rhs, .. } | HExprKind::Coalesce { lhs, rhs } => {
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
            HExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => {
                self.line(indent, &format!("let {}", pat_str(pattern)));
                self.expr(indent, expr);
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
            HExprKind::FStr(parts) => {
                for part in parts {
                    match part {
                        HFStrPart::Lit(s) => self.line(indent, &format!("lit {:?}", s)),
                        HFStrPart::Expr { expr, spec } => {
                            if let Some(spec) = spec {
                                self.line(indent, &format!("spec {:?}", spec));
                            }
                            self.expr(indent, expr);
                        }
                    }
                }
            }
            HExprKind::Try(inner) | HExprKind::Await(inner) => self.expr(indent, inner),
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

//! HIR → MIR lowering (Phase 3.2) — the **substrate** the rest of Phase 3 builds
//! on. It walks the typed, `BindingId`-based HIR ([`crate::hir`]) and emits a
//! control-flow graph of [`crate::mir`] basic blocks: straight-line code becomes
//! statements/rvalues over typed locals, and control flow (`if`/`loop`/`while`/
//! `for`/`break`-with-value/`continue`/`return`) becomes terminators between
//! blocks.
//!
//! **Scope (3.2).** This lowers the core; the harder constructs are deferred to
//! their own subparts and make a function **bail** (it is reported as skipped, not
//! emitted as a broken stub):
//!
//! - `match` → Phase 3.3 (decision trees).
//! - closures → Phase 3.4 (closure conversion).
//! - heap-collection literals (`List`/`Map`/`Set`) → need the runtime (Phase 4/6).
//! - `async`/`spawn`/`await`/`try`-`catch` → Phases 9/10.
//!
//! Ownership is **not** threaded here: reads lower to `Operand::Copy` placeholders;
//! Phase 3.5 rewrites the consuming ones to `Move` and the borrowing ones to `Ref`,
//! and inserts `Drop`s. Every successfully lowered function is run through
//! [`MirFn::validate`] before it is accepted.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::ast::{BinOp, BindingId, UnOp};
use crate::hir::*;
use crate::mir::*;
use crate::ty::{IntKind, Ty, display_ty};

/// An enum's variants in declaration order, each with its payload fields (an
/// optional name — `Some(_)` for a struct-variant field — and the field type).
/// This is the match-lowering counterpart of the type checker's
/// `enum_variants_resolved`; built once from the HIR for variant discriminants.
type VariantList = Vec<(String, Vec<(Option<String>, Ty)>)>;

/// The product of lowering: the functions that lowered, plus the ones skipped
/// (with the reason) so `la3 mir` can report honestly what 3.2 does not yet do.
pub(crate) struct LowerResult {
    pub program: MirProgram,
    pub skipped: Vec<(String, String)>,
}

/// Lower a whole HIR program to MIR.
pub(crate) fn lower(hir: &Hir) -> LowerResult {
    // Field declaration order per struct, for `Field`/`StructLit` projection.
    let mut struct_fields: HashMap<String, Vec<String>> = HashMap::new();
    for s in &hir.structs {
        struct_fields.insert(
            s.name.clone(),
            s.fields.iter().map(|(n, _)| n.clone()).collect(),
        );
    }
    // Variant order + payload types per enum, for match discriminants (3.3).
    let mut enums: HashMap<String, VariantList> = HashMap::new();
    for e in &hir.enums {
        let variants = e
            .variants
            .iter()
            .map(|v| {
                let fields = match &v.kind {
                    HVariantKind::Unit => Vec::new(),
                    HVariantKind::Tuple(tys) => tys.iter().map(|t| (None, t.clone())).collect(),
                    HVariantKind::Struct(fs) => fs
                        .iter()
                        .map(|(n, t)| (Some(n.clone()), t.clone()))
                        .collect(),
                };
                (v.name.clone(), fields)
            })
            .collect();
        enums.insert(e.name.clone(), variants);
    }
    let mut program = MirProgram { fns: Vec::new() };
    let mut skipped = Vec::new();
    for f in &hir.fns {
        let mut lo = FnLower::new(f, &struct_fields, &enums);
        match lo.run(f) {
            Ok(mir) => {
                // Validate the function and every closure lifted out of it; commit
                // them together (or skip the whole function on any invalid MIR).
                let lifted = std::mem::take(&mut lo.lifted);
                let mut errs: Vec<String> = mir.validate().err().unwrap_or_default();
                for lf in &lifted {
                    if let Err(es) = lf.validate() {
                        errs.extend(es.into_iter().map(|e| format!("{}: {}", lf.name, e)));
                    }
                }
                if errs.is_empty() {
                    program.fns.push(mir);
                    program.fns.extend(lifted);
                } else {
                    skipped.push((label(f), format!("invalid MIR: {}", errs.join("; "))));
                }
            }
            Err(reason) => skipped.push((label(f), reason)),
        }
    }
    LowerResult { program, skipped }
}

fn label(f: &HFn) -> String {
    match &f.owner {
        Some(o) => format!("{}::{}", o, f.name),
        None => f.name.clone(),
    }
}

/// Break/continue targets (and the value slot) of an enclosing loop.
struct LoopCtx {
    continue_to: BlockId,
    break_to: BlockId,
    /// `_0`-style slot a `break value` writes; `None` for value-less loops.
    break_val: Option<Local>,
}

type R<T> = Result<T, String>;

struct FnLower<'a> {
    b: MirBuilder,
    cur: BlockId,
    binding_local: HashMap<BindingId, Local>,
    /// Captures of a lifted closure body, resolved to places inside the env
    /// parameter (`_1`); checked before [`Self::binding_local`]. Empty for a
    /// normal (non-closure) function. Populated by [`FnLower::new_closure`].
    capture_place: HashMap<BindingId, Place>,
    loops: Vec<LoopCtx>,
    struct_fields: &'a HashMap<String, Vec<String>>,
    enums: &'a HashMap<String, VariantList>,
    /// This function's mangled symbol, the prefix for any closure it lifts.
    sym: String,
    /// Counter for naming the closures lifted out of this body (`{sym}::{{closure#n}}`).
    next_closure: u32,
    /// Functions lifted out of this body (closures and their nested closures).
    /// Committed to the program alongside this function once it lowers cleanly.
    lifted: Vec<MirFn>,
}

impl<'a> FnLower<'a> {
    fn new(
        f: &HFn,
        struct_fields: &'a HashMap<String, Vec<String>>,
        enums: &'a HashMap<String, VariantList>,
    ) -> Self {
        let mut args: Vec<(Ty, Option<String>)> = f
            .params
            .iter()
            .map(|p| (p.ty.clone(), Some(p.name.clone())))
            .collect();
        if let Some(v) = &f.variadic {
            args.push((v.ty.clone(), Some(v.name.clone())));
        }
        let mut b = MirBuilder::new(f.name.clone(), f.owner.clone(), f.ret.clone(), args);
        let mut binding_local = HashMap::new();
        let mut idx = 1u32; // _0 is the return slot; args are _1..
        for p in &f.params {
            binding_local.insert(p.binding, Local(idx));
            idx += 1;
        }
        if let Some(v) = &f.variadic {
            binding_local.insert(v.binding, Local(idx));
        }
        let cur = b.new_block();
        FnLower {
            b,
            cur,
            binding_local,
            capture_place: HashMap::new(),
            loops: Vec::new(),
            struct_fields,
            enums,
            sym: label(f),
            next_closure: 0,
            lifted: Vec::new(),
        }
    }

    /// A [`FnLower`] for a **lifted closure body** (Phase 3.4). Its first
    /// parameter `_1` is the captured environment (a tuple); the closure's own
    /// parameters follow as `_2..`. Each capture resolves to a place inside `_1`
    /// — `_1.i` for a by-value capture, `(*_1.i)` for a by-reference one (the env
    /// field is then a `&T`), so reading/writing the capture goes through the env.
    fn new_closure(
        sym: &str,
        env_ty: Ty,
        params: &[HParam],
        captures: &[HCapture],
        ret: Ty,
        struct_fields: &'a HashMap<String, Vec<String>>,
        enums: &'a HashMap<String, VariantList>,
    ) -> Self {
        let mut args: Vec<(Ty, Option<String>)> = vec![(env_ty, Some("env".into()))];
        for p in params {
            args.push((p.ty.clone(), Some(p.name.clone())));
        }
        let b = MirBuilder::new(sym.to_string(), None, ret, args);
        let mut binding_local = HashMap::new();
        // _0 = return, _1 = env, params are _2..
        let mut idx = 2u32;
        for p in params {
            binding_local.insert(p.binding, Local(idx));
            idx += 1;
        }
        // Captures map to projections of the env parameter `_1`.
        let mut capture_place = HashMap::new();
        for (i, c) in captures.iter().enumerate() {
            let mut place = Place::local(Local(1));
            place.proj.push(Projection::Field(i));
            if c.mode == CaptureMode::Ref {
                place.proj.push(Projection::Deref);
            }
            capture_place.insert(c.binding, place);
        }
        let mut lo = FnLower {
            b,
            cur: BlockId(0),
            binding_local,
            capture_place,
            loops: Vec::new(),
            struct_fields,
            enums,
            sym: sym.to_string(),
            next_closure: 0,
            lifted: Vec::new(),
        };
        lo.cur = lo.b.new_block();
        lo
    }

    fn run(&mut self, f: &HFn) -> R<MirFn> {
        let body = self.lower_block(&f.body)?;
        // The body's tail value is the function's result; write it and return.
        self.emit(Statement::Assign(
            Place::local(MirFn::return_local()),
            Rvalue::Use(body),
        ));
        self.b.set_term(self.cur, Terminator::Return);
        Ok(std::mem::replace(&mut self.b, MirBuilder::new("", None, Ty::Unit, vec![])).finish())
    }

    // -- emission helpers --------------------------------------------------

    fn emit(&mut self, s: Statement) {
        self.b.push_stmt(self.cur, s);
    }

    /// Close `cur` with `t` and open a fresh block to continue in (used after a
    /// diverging `return`/`break`/`continue`, so following code has a home).
    fn diverge(&mut self, t: Terminator) {
        self.b.set_term(self.cur, t);
        self.cur = self.b.new_block();
    }

    fn temp(&mut self, ty: Ty) -> Local {
        self.b.new_temp(ty)
    }

    // -- blocks / statements ----------------------------------------------

    /// Lower a block; the returned operand is the block's value (its tail, or
    /// `()` when there is none).
    fn lower_block(&mut self, blk: &HBlock) -> R<Operand> {
        for s in &blk.stmts {
            self.lower_stmt(s)?;
        }
        match &blk.tail {
            Some(e) => self.lower_operand(e),
            None => Ok(Operand::Const(Const::Unit)),
        }
    }

    fn lower_stmt(&mut self, s: &HStmt) -> R<()> {
        match s {
            HStmt::Let {
                pattern, ty, value, ..
            } => match pattern {
                HPattern::Binding(bid) => {
                    let local = self.b.new_local(ty.clone(), LocalKind::User, None);
                    let rv = self.lower_rvalue(value)?;
                    self.emit(Statement::Assign(Place::local(local), rv));
                    // Bind after lowering the value (matches HIR/resolution order).
                    self.binding_local.insert(*bid, local);
                    Ok(())
                }
                HPattern::Wildcard => {
                    let _ = self.lower_operand(value)?; // for effect
                    Ok(())
                }
                _ => Err("`let` destructuring pattern not lowered yet (Phase 3.3)".into()),
            },
            HStmt::Expr(e) => {
                let _ = self.lower_operand(e)?;
                Ok(())
            }
            HStmt::Return(e, _) => {
                let v = match e {
                    Some(e) => self.lower_operand(e)?,
                    None => Operand::Const(Const::Unit),
                };
                self.emit(Statement::Assign(
                    Place::local(MirFn::return_local()),
                    Rvalue::Use(v),
                ));
                self.diverge(Terminator::Return);
                Ok(())
            }
            HStmt::Break(e, _) => {
                let ctx = self
                    .loops
                    .last()
                    .ok_or_else(|| "`break` outside a loop".to_string())?;
                let (break_to, slot) = (ctx.break_to, ctx.break_val);
                if let Some(e) = e {
                    let v = self.lower_operand(e)?;
                    let slot =
                        slot.ok_or_else(|| "`break value` in a value-less loop".to_string())?;
                    self.emit(Statement::Assign(Place::local(slot), Rvalue::Use(v)));
                }
                self.diverge(Terminator::Goto(break_to));
                Ok(())
            }
            HStmt::Continue(_) => {
                let to = self
                    .loops
                    .last()
                    .ok_or_else(|| "`continue` outside a loop".to_string())?
                    .continue_to;
                self.diverge(Terminator::Goto(to));
                Ok(())
            }
            HStmt::Fn(_) | HStmt::Const(_) => Err("nested fn/const item not lowered yet".into()),
        }
    }

    // -- expressions: operand / rvalue / place ----------------------------

    /// Lower `e` to an [`Operand`] holding its value, emitting whatever
    /// statements/blocks that takes.
    fn lower_operand(&mut self, e: &HExpr) -> R<Operand> {
        use HExprKind::*;
        match &e.kind {
            Int(v) => Ok(Operand::Const(Const::Int(*v, e.ty.clone()))),
            Float(v) => Ok(Operand::Const(Const::Float(*v))),
            Bool(b) => Ok(Operand::Const(Const::Bool(*b))),
            Char(c) => Ok(Operand::Const(Const::Char(*c))),
            Str(s) => Ok(Operand::Const(Const::Str(s.clone()))),
            Nil => Ok(Operand::Const(Const::Nil)),
            Local(bid) => Ok(Operand::Copy(self.binding_place(*bid)?)),
            Global(name) => Ok(Operand::Const(Const::Fn(name.clone()))),
            Path(segs) => Ok(Operand::Const(Const::Fn(segs.join("::")))),

            // `module.CONST` (a field on a module path, e.g. `math.pi`) is a
            // qualified global *value*, not a place projection — mirror how a bare
            // `Global` lowers (a named global reference the back-end resolves).
            Field { recv, name } if matches!(recv.kind, Global(_)) => {
                let module = match &recv.kind {
                    Global(m) => m.clone(),
                    _ => unreachable!(),
                };
                Ok(Operand::Const(Const::Fn(format!("{}.{}", module, name))))
            }
            Field { .. } | Index { .. } => Ok(Operand::Copy(self.lower_place(e)?)),

            // Compound value-producing expressions go through an rvalue + temp.
            Binary { .. } | Unary { .. } | Cast { .. } | Tuple(_) | StructLit { .. } => {
                let rv = self.lower_rvalue(e)?;
                let t = self.temp(e.ty.clone());
                self.emit(Statement::Assign(Place::local(t), rv));
                Ok(Operand::Copy(Place::local(t)))
            }

            Call { .. } | MethodCall { .. } | Format { .. } => self.lower_call_like(e),

            // `target = value` is an expression yielding `()`.
            Assign { target, value } => {
                let rv = self.lower_rvalue(value)?;
                let place = self.lower_place(target)?;
                self.emit(Statement::Assign(place, rv));
                Ok(Operand::Const(Const::Unit))
            }

            Block(b) => self.lower_block(b),
            If { .. } => self.lower_if(e),
            Loop { .. } => self.lower_loop(e),
            While { cond, body } => {
                self.lower_while(cond, body)?;
                Ok(Operand::Const(Const::Unit))
            }
            For {
                pattern,
                iter,
                body,
            } => {
                self.lower_for(pattern, iter, body)?;
                Ok(Operand::Const(Const::Unit))
            }
            Unsafe(b) => self.lower_block(b),

            Match { .. } => self.lower_match(e),
            Closure { .. } => self.lower_closure(e),
            List(_) | Set(_) | Map(_) | ListRepeat { .. } => {
                Err("heap-collection literals need the runtime (Phase 4/6)".into())
            }
            Range { .. } => Err("range value not lowered yet (only `for` ranges)".into()),
            Await(_) | Spawn(_) | TryCatch { .. } => {
                Err("async/spawn/try-catch not lowered yet (Phases 9/10)".into())
            }
        }
    }

    /// Lower `e` directly to an [`Rvalue`] (the right-hand side of an assignment),
    /// avoiding a redundant temporary for the common compound shapes.
    fn lower_rvalue(&mut self, e: &HExpr) -> R<Rvalue> {
        use HExprKind::*;
        match &e.kind {
            Binary { op, lhs, rhs } => {
                let a = self.lower_operand(lhs)?;
                let b = self.lower_operand(rhs)?;
                Ok(Rvalue::Binary(*op, a, b))
            }
            Unary { op, expr } => match op {
                UnOp::Ref | UnOp::RefMut | UnOp::RawRef => Ok(Rvalue::Ref(self.lower_place(expr)?)),
                UnOp::Deref => Ok(Rvalue::Use(Operand::Copy(self.lower_place(e)?))),
                UnOp::Neg | UnOp::Not | UnOp::BitNot => {
                    Ok(Rvalue::Unary(*op, self.lower_operand(expr)?))
                }
            },
            Cast { expr, ty } => Ok(Rvalue::Cast(self.lower_operand(expr)?, ty.clone())),
            Tuple(xs) => {
                let ops = self.lower_operands(xs)?;
                Ok(Rvalue::Aggregate(AggregateKind::Tuple, ops))
            }
            StructLit {
                name,
                fields,
                spread,
            } => {
                if spread.is_some() {
                    return Err("struct literal `..spread` not lowered yet".into());
                }
                let order = self
                    .struct_fields
                    .get(name)
                    .ok_or_else(|| format!("unknown struct `{}`", name))?
                    .clone();
                let mut ops = Vec::with_capacity(order.len());
                for fname in &order {
                    let (_, fe) = fields
                        .iter()
                        .find(|(n, _)| n == fname)
                        .ok_or_else(|| format!("missing field `{}` in `{}`", fname, name))?;
                    ops.push(self.lower_operand(fe)?);
                }
                Ok(Rvalue::Aggregate(AggregateKind::Struct(name.clone()), ops))
            }
            // Anything else: lower to an operand and use it.
            _ => Ok(Rvalue::Use(self.lower_operand(e)?)),
        }
    }

    fn lower_operands(&mut self, es: &[HExpr]) -> R<Vec<Operand>> {
        es.iter().map(|e| self.lower_operand(e)).collect()
    }

    /// Lower an lvalue expression to a [`Place`].
    fn lower_place(&mut self, e: &HExpr) -> R<Place> {
        use HExprKind::*;
        match &e.kind {
            Local(bid) => self.binding_place(*bid),
            Field { recv, name } => {
                let mut p = self.lower_place(recv)?;
                p.proj
                    .push(Projection::Field(self.field_index(&recv.ty, name)?));
                Ok(p)
            }
            Index { recv, index } => {
                let mut p = self.lower_place(recv)?;
                let iop = self.lower_operand(index)?;
                let il = self.into_local(iop, index.ty.clone());
                p.proj.push(Projection::Index(il));
                Ok(p)
            }
            Unary {
                op: UnOp::Deref,
                expr,
            } => {
                let mut p = self.lower_place(expr)?;
                p.proj.push(Projection::Deref);
                Ok(p)
            }
            _ => Err("expression is not an assignable place".into()),
        }
    }

    // -- calls -------------------------------------------------------------

    fn lower_call_like(&mut self, e: &HExpr) -> R<Operand> {
        use HExprKind::*;
        let (func, args): (Operand, Vec<Operand>) = match &e.kind {
            Call { callee, args } => {
                let func = match &callee.kind {
                    Global(name) => Operand::Const(Const::Fn(name.clone())),
                    Path(segs) => Operand::Const(Const::Fn(segs.join("::"))),
                    // An indirect call through a closure/fn value needs the env-
                    // passing closure ABI, which lands with codegen in Phase 8.
                    _ => {
                        return Err(
                            "indirect call through a closure/fn value not lowered yet (Phase 8 closure ABI)".into(),
                        );
                    }
                };
                (func, self.lower_operands(args)?)
            }
            MethodCall { recv, method, args } => {
                // A call on a module path (`io.println`) is a free function in that
                // module; a call on a value (`x.len()`) is `Type::method(self, ..)`.
                let (func, mut ops) = match &recv.kind {
                    Global(module) => (
                        Operand::Const(Const::Fn(format!("{}.{}", module, method))),
                        Vec::new(),
                    ),
                    _ => {
                        let recv_op = self.lower_operand(recv)?;
                        let sym = format!("{}::{}", ty_symbol(&recv.ty), method);
                        (Operand::Const(Const::Fn(sym)), vec![recv_op])
                    }
                };
                ops.extend(self.lower_operands(args)?);
                (func, ops)
            }
            Format { value, spec } => {
                let v = self.lower_operand(value)?;
                let mut ops = vec![v];
                if let Some(s) = spec {
                    ops.push(Operand::Const(Const::Str(s.clone())));
                }
                (Operand::Const(Const::Fn("std::format".into())), ops)
            }
            _ => unreachable!("lower_call_like on a non-call"),
        };
        let dest = self.temp(e.ty.clone());
        let next = self.b.new_block();
        self.b.set_term(
            self.cur,
            Terminator::Call {
                func,
                args,
                dest: Some((Place::local(dest), next)),
            },
        );
        self.cur = next;
        Ok(Operand::Copy(Place::local(dest)))
    }

    // -- control flow ------------------------------------------------------

    fn lower_if(&mut self, e: &HExpr) -> R<Operand> {
        let (cond, then, els) = match &e.kind {
            HExprKind::If { cond, then, els } => (cond, then, els),
            _ => unreachable!(),
        };
        let c = self.lower_operand(cond)?;
        let result = self.temp(e.ty.clone());
        let then_b = self.b.new_block();
        let else_b = self.b.new_block();
        let join = self.b.new_block();
        self.b.set_term(
            self.cur,
            Terminator::If {
                cond: c,
                then_blk: then_b,
                else_blk: else_b,
            },
        );
        // then arm
        self.cur = then_b;
        let tv = self.lower_block(then)?;
        self.emit(Statement::Assign(Place::local(result), Rvalue::Use(tv)));
        self.b.set_term(self.cur, Terminator::Goto(join));
        // else arm
        self.cur = else_b;
        let ev = match els {
            Some(e) => self.lower_operand(e)?,
            None => Operand::Const(Const::Unit),
        };
        self.emit(Statement::Assign(Place::local(result), Rvalue::Use(ev)));
        self.b.set_term(self.cur, Terminator::Goto(join));
        self.cur = join;
        Ok(Operand::Copy(Place::local(result)))
    }

    fn lower_loop(&mut self, e: &HExpr) -> R<Operand> {
        let body = match &e.kind {
            HExprKind::Loop { body } => body,
            _ => unreachable!(),
        };
        let header = self.b.new_block();
        let join = self.b.new_block();
        let result = self.temp(e.ty.clone());
        self.b.set_term(self.cur, Terminator::Goto(header));
        self.cur = header;
        self.loops.push(LoopCtx {
            continue_to: header,
            break_to: join,
            break_val: Some(result),
        });
        let _ = self.lower_block(body)?;
        self.b.set_term(self.cur, Terminator::Goto(header));
        self.loops.pop();
        self.cur = join;
        Ok(Operand::Copy(Place::local(result)))
    }

    fn lower_while(&mut self, cond: &HExpr, body: &HBlock) -> R<()> {
        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let join = self.b.new_block();
        self.b.set_term(self.cur, Terminator::Goto(header));
        self.cur = header;
        let c = self.lower_operand(cond)?;
        self.b.set_term(
            self.cur,
            Terminator::If {
                cond: c,
                then_blk: body_b,
                else_blk: join,
            },
        );
        self.cur = body_b;
        self.loops.push(LoopCtx {
            continue_to: header,
            break_to: join,
            break_val: None,
        });
        let _ = self.lower_block(body)?;
        self.b.set_term(self.cur, Terminator::Goto(header));
        self.loops.pop();
        self.cur = join;
        Ok(())
    }

    /// `for v in start..end { body }` lowered to a counter loop. Only `Range`
    /// iterables are handled in 3.2 (List/Map/Set iteration needs the runtime).
    fn lower_for(&mut self, pattern: &HPattern, iter: &HExpr, body: &HBlock) -> R<()> {
        let (start, end, inclusive) = match &iter.kind {
            HExprKind::Range {
                start,
                end,
                inclusive,
            } => (start, end, *inclusive),
            _ => return Err("`for` over a non-range iterable not lowered yet (Phase 6)".into()),
        };
        let var = match pattern {
            HPattern::Binding(b) => *b,
            _ => return Err("`for` with a destructuring pattern not lowered yet".into()),
        };
        let ity = start.ty.clone();
        // i = start; end_l = end
        let i = self.b.new_local(ity.clone(), LocalKind::User, None);
        self.binding_local.insert(var, i);
        let start_op = self.lower_operand(start)?;
        self.emit(Statement::Assign(Place::local(i), Rvalue::Use(start_op)));
        let end_op = self.lower_operand(end)?;
        let end_l = self.into_local(end_op, ity.clone());

        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let incr = self.b.new_block();
        let join = self.b.new_block();
        self.b.set_term(self.cur, Terminator::Goto(header));
        // header: cmp = i </<= end ; if cmp -> [body, join]
        self.cur = header;
        let cmp = self.temp(Ty::Bool);
        let op = if inclusive { BinOp::Le } else { BinOp::Lt };
        self.emit(Statement::Assign(
            Place::local(cmp),
            Rvalue::Binary(
                op,
                Operand::Copy(Place::local(i)),
                Operand::Copy(Place::local(end_l)),
            ),
        ));
        self.b.set_term(
            self.cur,
            Terminator::If {
                cond: Operand::Copy(Place::local(cmp)),
                then_blk: body_b,
                else_blk: join,
            },
        );
        // body
        self.cur = body_b;
        self.loops.push(LoopCtx {
            continue_to: incr,
            break_to: join,
            break_val: None,
        });
        let _ = self.lower_block(body)?;
        self.b.set_term(self.cur, Terminator::Goto(incr));
        self.loops.pop();
        // incr: i = i + 1 ; goto header
        self.cur = incr;
        self.emit(Statement::Assign(
            Place::local(i),
            Rvalue::Binary(
                BinOp::Add,
                Operand::Copy(Place::local(i)),
                Operand::Const(Const::Int(1, ity)),
            ),
        ));
        self.b.set_term(self.cur, Terminator::Goto(header));
        self.cur = join;
        Ok(())
    }

    // -- match → decision tree (3.3) --------------------------------------

    /// Lower a `match` to a decision tree. Arms are tested top-to-bottom, first
    /// match wins (mirroring the interpreter oracle): each arm emits a chain of
    /// tests routing to its body block on success or to the *next arm* on failure,
    /// and the body's value is threaded into a result temp. Because `match` is
    /// exhaustive (reference Section 7), the fall-through past the last arm is
    /// statically [`Terminator::Unreachable`].
    fn lower_match(&mut self, e: &HExpr) -> R<Operand> {
        let (scrut, arms) = match &e.kind {
            HExprKind::Match { scrutinee, arms } => (scrutinee, arms),
            _ => unreachable!(),
        };
        // Materialize the scrutinee so the patterns can re-read and project it.
        let scrut_op = self.lower_operand(scrut)?;
        let sty = scrut.ty.clone();
        let scrut_local = self.into_local(scrut_op, sty.clone());
        let scrut_place = Place::local(scrut_local);
        let result = self.temp(e.ty.clone());
        let join = self.b.new_block();
        for arm in arms {
            let body_blk = self.b.new_block();
            let next_blk = self.b.new_block();
            self.test_pattern(&arm.pattern, &scrut_place, &sty, body_blk, next_blk)?;
            // The arm body, gated by its guard (a failed guard falls to next arm).
            self.cur = body_blk;
            if let Some(guard) = &arm.guard {
                let g = self.lower_operand(guard)?;
                let run = self.b.new_block();
                self.b.set_term(
                    self.cur,
                    Terminator::If {
                        cond: g,
                        then_blk: run,
                        else_blk: next_blk,
                    },
                );
                self.cur = run;
            }
            let bv = self.lower_operand(&arm.body)?;
            self.emit(Statement::Assign(Place::local(result), Rvalue::Use(bv)));
            self.b.set_term(self.cur, Terminator::Goto(join));
            self.cur = next_blk;
        }
        self.b.set_term(self.cur, Terminator::Unreachable);
        self.cur = join;
        Ok(Operand::Copy(Place::local(result)))
    }

    /// Emit, starting at `self.cur`, the tests for `pat` against `place` (of type
    /// `ty`). Bindings are established on the success path; control reaches
    /// `success` when the pattern matches and `fail` otherwise. The function
    /// always terminates `self.cur`.
    fn test_pattern(
        &mut self,
        pat: &HPattern,
        place: &Place,
        ty: &Ty,
        success: BlockId,
        fail: BlockId,
    ) -> R<()> {
        match pat {
            HPattern::Wildcard => {
                self.b.set_term(self.cur, Terminator::Goto(success));
            }
            HPattern::Binding(bid) => {
                self.bind_place(*bid, place, ty);
                self.b.set_term(self.cur, Terminator::Goto(success));
            }
            HPattern::At(bid, sub) => {
                // Test the sub-pattern; bind the whole value only once it matches.
                let bind_blk = self.b.new_block();
                self.test_pattern(sub, place, ty, bind_blk, fail)?;
                self.cur = bind_blk;
                self.bind_place(*bid, place, ty);
                self.b.set_term(self.cur, Terminator::Goto(success));
            }
            HPattern::Int(n) => self.switch_eq(place, *n as i128, success, fail),
            HPattern::Char(c) => self.switch_eq(place, *c as i128, success, fail),
            HPattern::Bool(b) => {
                // Two-way branch on the bool value itself.
                let (then_blk, else_blk) = if *b { (success, fail) } else { (fail, success) };
                self.b.set_term(
                    self.cur,
                    Terminator::If {
                        cond: Operand::Copy(place.clone()),
                        then_blk,
                        else_blk,
                    },
                );
            }
            HPattern::Str(s) => {
                // Structural string equality; the runtime `str` eq lands in Phase 4,
                // the `Binary(Eq, …)` is valid MIR meanwhile.
                let eq = self.temp(Ty::Bool);
                self.emit(Statement::Assign(
                    Place::local(eq),
                    Rvalue::Binary(
                        BinOp::Eq,
                        Operand::Copy(place.clone()),
                        Operand::Const(Const::Str(s.clone())),
                    ),
                ));
                self.b.set_term(
                    self.cur,
                    Terminator::If {
                        cond: Operand::Copy(Place::local(eq)),
                        then_blk: success,
                        else_blk: fail,
                    },
                );
            }
            HPattern::Range { lo, hi, inclusive } => {
                // lo <= place && place </<= hi, as two chained comparisons.
                let ge = self.temp(Ty::Bool);
                self.emit(Statement::Assign(
                    Place::local(ge),
                    Rvalue::Binary(
                        BinOp::Ge,
                        Operand::Copy(place.clone()),
                        Operand::Const(Const::Int(*lo, ty.clone())),
                    ),
                ));
                let chk_hi = self.b.new_block();
                self.b.set_term(
                    self.cur,
                    Terminator::If {
                        cond: Operand::Copy(Place::local(ge)),
                        then_blk: chk_hi,
                        else_blk: fail,
                    },
                );
                self.cur = chk_hi;
                let le = self.temp(Ty::Bool);
                let op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                self.emit(Statement::Assign(
                    Place::local(le),
                    Rvalue::Binary(
                        op,
                        Operand::Copy(place.clone()),
                        Operand::Const(Const::Int(*hi, ty.clone())),
                    ),
                ));
                self.b.set_term(
                    self.cur,
                    Terminator::If {
                        cond: Operand::Copy(Place::local(le)),
                        then_blk: success,
                        else_blk: fail,
                    },
                );
            }
            HPattern::Tuple(subs) => {
                let elems = match ty {
                    Ty::Tuple(es) => es.clone(),
                    _ => return Err(format!("tuple pattern on non-tuple {}", display_ty(ty))),
                };
                let parts: Vec<(&HPattern, Place, Ty)> = subs
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let mut sp = place.clone();
                        sp.proj.push(Projection::Field(i));
                        (p, sp, elems.get(i).cloned().unwrap_or(Ty::Unknown))
                    })
                    .collect();
                self.test_seq(parts, success, fail)?;
            }
            HPattern::Variant { path, args } => {
                let variant = path.last().map(|s| s.as_str()).unwrap_or("");
                let (vidx, payload) = self.switch_variant(place, ty, variant, fail)?;
                let parts: Vec<(&HPattern, Place, Ty)> = args
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let mut sp = place.clone();
                        sp.proj.push(Projection::Downcast(vidx));
                        sp.proj.push(Projection::Field(i));
                        let fty = payload
                            .get(i)
                            .map(|(_, t)| t.clone())
                            .unwrap_or(Ty::Unknown);
                        (p, sp, fty)
                    })
                    .collect();
                self.test_seq(parts, success, fail)?;
            }
            HPattern::Struct { name, fields } => {
                // An enum struct-variant (`Shape.Rect { width, height }`); the
                // plain-struct destructure on a `Ty::Struct` is deferred.
                let variant = name.rsplit('.').next().unwrap_or(name.as_str());
                let (vidx, payload) = self.switch_variant(place, ty, variant, fail)?;
                for (fname, bid) in fields {
                    let fidx = payload
                        .iter()
                        .position(|(n, _)| n.as_deref() == Some(fname.as_str()))
                        .ok_or_else(|| format!("variant `{}` has no field `{}`", variant, fname))?;
                    let mut sp = place.clone();
                    sp.proj.push(Projection::Downcast(vidx));
                    sp.proj.push(Projection::Field(fidx));
                    self.bind_place(*bid, &sp, &payload[fidx].1);
                }
                self.b.set_term(self.cur, Terminator::Goto(success));
            }
            HPattern::Or(alts) => {
                // Try each alternative; a failed one falls to the next, the last
                // to `fail`. Each alternative routes its own success to `success`.
                let mut entry = self.cur;
                let n = alts.len();
                for (i, alt) in alts.iter().enumerate() {
                    let next_alt = if i + 1 == n { fail } else { self.b.new_block() };
                    self.cur = entry;
                    self.test_pattern(alt, place, ty, success, next_alt)?;
                    entry = next_alt;
                }
            }
            HPattern::Nil => {
                return Err(
                    "nil/union pattern not lowered yet (needs union representation, Phase 6)"
                        .into(),
                );
            }
            HPattern::List { .. } => {
                return Err("list pattern not lowered yet (needs the runtime, Phase 6)".into());
            }
            HPattern::Typed { .. } => {
                return Err("typed (union-narrowing) pattern not lowered yet (Phase 6)".into());
            }
        }
        Ok(())
    }

    /// Test a sequence of sub-patterns (tuple elements / variant payload) in
    /// order: each that matches flows to the next, the last to `success`; any
    /// failure flows to `fail`.
    fn test_seq(
        &mut self,
        parts: Vec<(&HPattern, Place, Ty)>,
        success: BlockId,
        fail: BlockId,
    ) -> R<()> {
        if parts.is_empty() {
            self.b.set_term(self.cur, Terminator::Goto(success));
            return Ok(());
        }
        let n = parts.len();
        let mut entry = self.cur;
        for (i, (sub, sp, st)) in parts.into_iter().enumerate() {
            let this_success = if i + 1 == n {
                success
            } else {
                self.b.new_block()
            };
            self.cur = entry;
            self.test_pattern(sub, &sp, &st, this_success, fail)?;
            entry = this_success;
        }
        Ok(())
    }

    /// `switch place { val => success, _ => fail }` over an integer/char value.
    fn switch_eq(&mut self, place: &Place, val: i128, success: BlockId, fail: BlockId) {
        self.b.set_term(
            self.cur,
            Terminator::Switch {
                discr: Operand::Copy(place.clone()),
                targets: vec![(val, success)],
                default: fail,
            },
        );
    }

    /// Read `place`'s enum discriminant and switch on the chosen `variant`:
    /// matching flows to a fresh block (left in `self.cur`), the default to
    /// `fail`. Returns the variant's index and resolved payload fields so the
    /// caller can descend into the payload via [`Projection::Downcast`].
    fn switch_variant(
        &mut self,
        place: &Place,
        ty: &Ty,
        variant: &str,
        fail: BlockId,
    ) -> R<(usize, Vec<(Option<String>, Ty)>)> {
        let ename = match ty {
            Ty::Enum(n, _) => n.clone(),
            _ => {
                return Err(format!(
                    "variant pattern `{}` on non-enum {}",
                    variant,
                    display_ty(ty)
                ));
            }
        };
        let (vidx, payload) = self.variant_info(&ename, ty, variant)?;
        let disc = self.temp(Ty::Int(IntKind::I32));
        self.emit(Statement::Assign(
            Place::local(disc),
            Rvalue::Discriminant(place.clone()),
        ));
        let matched = self.b.new_block();
        self.b.set_term(
            self.cur,
            Terminator::Switch {
                discr: Operand::Copy(Place::local(disc)),
                targets: vec![(vidx as i128, matched)],
                default: fail,
            },
        );
        self.cur = matched;
        Ok((vidx, payload))
    }

    /// Resolve a variant to its discriminant index and payload field types. The
    /// built-in `Option`/`Result` (whose variants are not declared in source)
    /// take their payload from the scrutinee's type arguments, matching the type
    /// checker's `enum_variants_resolved`.
    fn variant_info(
        &self,
        ename: &str,
        ty: &Ty,
        variant: &str,
    ) -> R<(usize, Vec<(Option<String>, Ty)>)> {
        let args: &[Ty] = match ty {
            Ty::Enum(_, a) => a,
            _ => &[],
        };
        let list: VariantList = match ename {
            "Option" => vec![
                ("None".into(), vec![]),
                (
                    "Some".into(),
                    vec![(None, args.first().cloned().unwrap_or(Ty::Unknown))],
                ),
            ],
            "Result" => vec![
                (
                    "Ok".into(),
                    vec![(None, args.first().cloned().unwrap_or(Ty::Unknown))],
                ),
                ("Err".into(), vec![(None, Ty::Str)]),
            ],
            _ => self
                .enums
                .get(ename)
                .cloned()
                .ok_or_else(|| format!("unknown enum `{}`", ename))?,
        };
        list.iter()
            .position(|(n, _)| n == variant)
            .map(|i| (i, list[i].1.clone()))
            .ok_or_else(|| format!("enum `{}` has no variant `{}`", ename, variant))
    }

    /// Bind `bid` to a fresh user local initialized from `place` (a copy for now;
    /// MIR 3.5 refines pattern bindings to moves where the borrow check allows).
    fn bind_place(&mut self, bid: BindingId, place: &Place, ty: &Ty) {
        let l = self.b.new_local(ty.clone(), LocalKind::User, None);
        self.emit(Statement::Assign(
            Place::local(l),
            Rvalue::Use(Operand::Copy(place.clone())),
        ));
        self.binding_local.insert(bid, l);
    }

    // -- closure conversion (3.4) -----------------------------------------

    /// Lower a `Closure` HIR node: **lift** its body into a synthetic top-level
    /// MIR function (env parameter + the closure's own parameters) and
    /// **materialize** the `{fn ptr, env}` value at the site. The capture list
    /// (Phase 2.5) decides each captured variable's env slot: a by-reference
    /// capture becomes `&place`, a by-value capture the value itself. Calling
    /// *through* the resulting value (the env-passing ABI) is Phase 8.
    fn lower_closure(&mut self, e: &HExpr) -> R<Operand> {
        let (params, captures, body) = match &e.kind {
            HExprKind::Closure {
                params,
                captures,
                body,
                ..
            } => (params, captures, body),
            _ => unreachable!(),
        };
        let name = format!("{}::{{closure#{}}}", self.sym, self.next_closure);
        self.next_closure += 1;

        // Lift the body into its own function; commit it (and any nested closures)
        // only once the whole parent lowers cleanly.
        let (lifted_fn, nested) = self.lift_closure(&name, params, captures, body)?;
        self.lifted.extend(nested);
        self.lifted.push(lifted_fn);

        // Build the captured environment at the site, in capture order.
        let mut env_ops = Vec::with_capacity(captures.len());
        for c in captures {
            let place = self.binding_place(c.binding)?;
            let op = match c.mode {
                CaptureMode::Ref => {
                    let r = self.temp(Ty::Ref(Box::new(c.ty.clone())));
                    self.emit(Statement::Assign(Place::local(r), Rvalue::Ref(place)));
                    Operand::Copy(Place::local(r))
                }
                CaptureMode::Value => Operand::Copy(place),
            };
            env_ops.push(op);
        }
        let val = self.temp(e.ty.clone());
        self.emit(Statement::Assign(
            Place::local(val),
            Rvalue::Aggregate(AggregateKind::Closure(name), env_ops),
        ));
        Ok(Operand::Copy(Place::local(val)))
    }

    /// Lower a closure body into a standalone [`MirFn`]. The env parameter `_1`
    /// is a tuple of the captured slots (`&T` for a by-ref capture, `T` for a
    /// by-value one); the closure's parameters follow. Returns the lifted function
    /// plus any functions lifted from closures nested inside it.
    fn lift_closure(
        &mut self,
        name: &str,
        params: &[HParam],
        captures: &[HCapture],
        body: &HExpr,
    ) -> R<(MirFn, Vec<MirFn>)> {
        let env_field_tys: Vec<Ty> = captures
            .iter()
            .map(|c| match c.mode {
                CaptureMode::Ref => Ty::Ref(Box::new(c.ty.clone())),
                CaptureMode::Value => c.ty.clone(),
            })
            .collect();
        let env_ty = Ty::Tuple(env_field_tys);
        let mut sub = FnLower::new_closure(
            name,
            env_ty,
            params,
            captures,
            body.ty.clone(),
            self.struct_fields,
            self.enums,
        );
        let val = sub.lower_operand(body)?;
        sub.emit(Statement::Assign(
            Place::local(MirFn::return_local()),
            Rvalue::Use(val),
        ));
        sub.b.set_term(sub.cur, Terminator::Return);
        let nested = std::mem::take(&mut sub.lifted);
        let mir = std::mem::replace(&mut sub.b, MirBuilder::new("", None, Ty::Unit, vec![])).finish();
        Ok((mir, nested))
    }

    // -- small helpers -----------------------------------------------------

    fn binding_place(&self, b: BindingId) -> R<Place> {
        if let Some(p) = self.capture_place.get(&b) {
            return Ok(p.clone());
        }
        self.binding_local
            .get(&b)
            .map(|l| Place::local(*l))
            .ok_or_else(|| format!("unbound local #{}", b.0))
    }

    fn into_local(&mut self, op: Operand, ty: Ty) -> Local {
        let t = self.temp(ty);
        self.emit(Statement::Assign(Place::local(t), Rvalue::Use(op)));
        t
    }

    fn field_index(&self, recv_ty: &Ty, name: &str) -> R<usize> {
        // Tuple field (`.0`) is a numeric name.
        if let Ok(n) = name.parse::<usize>() {
            return Ok(n);
        }
        let sname = match recv_ty {
            Ty::Struct(n, _) => n.clone(),
            _ => return Err(format!("field `{}` on non-struct {:?}", name, recv_ty)),
        };
        let fields = self
            .struct_fields
            .get(&sname)
            .ok_or_else(|| format!("unknown struct `{}`", sname))?;
        fields
            .iter()
            .position(|f| f == name)
            .ok_or_else(|| format!("no field `{}` on `{}`", name, sname))
    }
}

/// A short type name used to synthesize a method symbol (`List::len`, `Point::area`).
fn ty_symbol(t: &Ty) -> String {
    match t {
        Ty::Struct(n, _) | Ty::Enum(n, _) => n.clone(),
        Ty::List(_) => "List".into(),
        Ty::Map(_, _) => "Map".into(),
        Ty::Set(_) => "Set".into(),
        Ty::Str => "str".into(),
        Ty::Ref(inner) | Ty::Ptr(inner) => ty_symbol(inner),
        other => crate::ty::display_ty(other),
    }
}

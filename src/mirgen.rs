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

use crate::ast::{BindingId, BinOp, UnOp};
use crate::hir::*;
use crate::mir::*;
use crate::ty::Ty;

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
        struct_fields.insert(s.name.clone(), s.fields.iter().map(|(n, _)| n.clone()).collect());
    }
    let mut program = MirProgram { fns: Vec::new() };
    let mut skipped = Vec::new();
    for f in &hir.fns {
        let mut lo = FnLower::new(f, &struct_fields);
        match lo.run(f) {
            Ok(mir) => match mir.validate() {
                Ok(()) => program.fns.push(mir),
                Err(es) => skipped.push((label(f), format!("invalid MIR: {}", es.join("; ")))),
            },
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
    loops: Vec<LoopCtx>,
    struct_fields: &'a HashMap<String, Vec<String>>,
}

impl<'a> FnLower<'a> {
    fn new(f: &HFn, struct_fields: &'a HashMap<String, Vec<String>>) -> Self {
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
            loops: Vec::new(),
            struct_fields,
        }
    }

    fn run(&mut self, f: &HFn) -> R<MirFn> {
        let body = self.lower_block(&f.body)?;
        // The body's tail value is the function's result; write it and return.
        self.emit(Statement::Assign(Place::local(MirFn::return_local()), Rvalue::Use(body)));
        self.b.set_term(self.cur, Terminator::Return);
        Ok(std::mem::replace(
            &mut self.b,
            MirBuilder::new("", None, Ty::Unit, vec![]),
        )
        .finish())
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
                pattern,
                ty,
                value,
                ..
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
                self.emit(Statement::Assign(Place::local(MirFn::return_local()), Rvalue::Use(v)));
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
                    let slot = slot.ok_or_else(|| "`break value` in a value-less loop".to_string())?;
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
            HStmt::Fn(_) | HStmt::Const(_) => {
                Err("nested fn/const item not lowered yet".into())
            }
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

            Match { .. } => Err("`match` not lowered until Phase 3.3".into()),
            Closure { .. } => Err("closures not lowered until Phase 3.4".into()),
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
                UnOp::Ref | UnOp::RefMut | UnOp::RawRef => {
                    Ok(Rvalue::Ref(self.lower_place(expr)?))
                }
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
                p.proj.push(Projection::Field(self.field_index(&recv.ty, name)?));
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
                    _ => self.lower_operand(callee)?, // indirect call through a value
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
            Rvalue::Binary(op, Operand::Copy(Place::local(i)), Operand::Copy(Place::local(end_l))),
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

    // -- small helpers -----------------------------------------------------

    fn binding_place(&self, b: BindingId) -> R<Place> {
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

//! Move analysis and closure capture move handling for ownership checking.

use super::support::{bind_fresh, closure_free_vars};
use super::*;

impl BorrowCk<'_> {
    // ---- statements ------------------------------------------------------

    pub(super) fn walk_block(&mut self, b: &Block, moved: &mut Moved) {
        for s in &b.stmts {
            self.walk_stmt(s, moved);
        }
        if let Some(t) = &b.tail {
            self.walk_expr(t, moved);
        }
    }

    pub(super) fn walk_stmt(&mut self, s: &Stmt, moved: &mut Moved) {
        match s {
            Stmt::Let { pattern, value, .. } => {
                self.walk_expr(value, moved);
                self.try_move(value, moved);
                // The freshly-bound names start owned again (re-init / shadow).
                bind_fresh(pattern, moved);
            }
            Stmt::Expr(e) => self.walk_expr(e, moved),
            Stmt::Return(opt, _) => {
                if let Some(e) = opt {
                    self.walk_expr(e, moved);
                    self.try_move(e, moved);
                }
            }
            Stmt::Break(opt, _) => {
                if let Some(e) = opt {
                    self.walk_expr(e, moved);
                    self.try_move(e, moved);
                }
            }
            Stmt::Continue(_) => {}
            Stmt::Item(Item::Fn(f)) => self.check_fn(f),
            Stmt::Item(_) => {}
        }
    }

    /// If `e` is a bare binding of a non-Copy type, mark it moved-out.
    pub(super) fn try_move(&mut self, e: &Expr, moved: &mut Moved) {
        if let ExprKind::Ident(name) = &e.kind {
            if !self.types.is_copy(e.id) {
                moved.insert(name.clone());
            }
        }
    }

    /// Move each argument whose matching parameter is taken by value.
    pub(super) fn move_by_value_args(
        &mut self,
        args: &[Expr],
        by_value: &[bool],
        moved: &mut Moved,
    ) {
        for (a, &by_val) in args.iter().zip(by_value) {
            if by_val {
                self.try_move(a, moved);
            }
        }
    }

    // ---- expressions -----------------------------------------------------

    pub(super) fn walk_expr(&mut self, e: &Expr, moved: &mut Moved) {
        match &e.kind {
            ExprKind::Ident(name) => {
                if moved.contains(name) {
                    self.err(
                        e.pos,
                        format!(
                            "use of moved value `{}`; it was moved out of this binding earlier \
                             and cannot be used again (its type is not `Copy`)",
                            name
                        ),
                    );
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::Nil
            | ExprKind::SelfExpr
            | ExprKind::Path(_) => {}

            ExprKind::FStr(parts) => {
                for p in parts {
                    if let FStrPart::Expr { expr, .. } = p {
                        self.walk_expr(expr, moved);
                    }
                }
            }
            ExprKind::Unary { expr, .. } => self.walk_expr(expr, moved),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
                self.walk_expr(lhs, moved);
                self.walk_expr(rhs, moved);
            }
            ExprKind::Assign { target, op, value } => {
                self.walk_expr(value, moved);
                if op.is_some() {
                    // Compound assignment reads the current target value.
                    self.walk_expr(target, moved);
                }
                self.try_move(value, moved);
                // A plain `=` to a bare binding re-initializes it.
                if op.is_none() {
                    if let ExprKind::Ident(n) = &target.kind {
                        moved.remove(n);
                    } else {
                        self.walk_expr(target, moved);
                    }
                }
            }
            ExprKind::Cast { expr, .. } | ExprKind::Try(expr) | ExprKind::Await(expr) => {
                self.walk_expr(expr, moved)
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, moved);
                for a in args {
                    self.walk_expr(a, moved);
                }
                // A bare-binding argument to a user function is moved when the
                // matching parameter is by value. Built-ins (absent here) borrow.
                if let ExprKind::Ident(name) = &callee.kind {
                    if let Some(flags) = self.sigs.free_fns.get(name).cloned() {
                        self.move_by_value_args(args, &flags, moved);
                    }
                }
            }
            ExprKind::MethodCall {
                recv, method, args, ..
            } => {
                self.walk_expr(recv, moved);
                for a in args {
                    self.walk_expr(a, moved);
                }
                // Resolve the receiver's type. `Type.assoc(..)` (recv is a known
                // type name) has no value receiver; otherwise the receiver is a
                // value whose type the checker recorded.
                let type_qualified = matches!(&recv.kind,
                    ExprKind::Ident(n) if self.sigs.type_names.contains(n));
                let tyname = if type_qualified {
                    match &recv.kind {
                        ExprKind::Ident(n) => Some(n.clone()),
                        _ => None,
                    }
                } else {
                    self.types
                        .type_of(recv.id)
                        .map(|t| head_name(&t).to_string())
                };
                if let Some(ty) = tyname {
                    if let Some((self_kind, flags)) =
                        self.sigs.methods.get(&(ty, method.clone())).cloned()
                    {
                        // A consuming method called on a value moves the receiver.
                        if self_kind == SelfKind::Value && !type_qualified {
                            self.try_move(recv, moved);
                        }
                        self.move_by_value_args(args, &flags, moved);
                    }
                }
            }
            ExprKind::Field { recv, .. } => self.walk_expr(recv, moved),
            ExprKind::Index { recv, index } => {
                self.walk_expr(recv, moved);
                self.walk_expr(index, moved);
            }
            ExprKind::Tuple(xs) | ExprKind::List(xs) | ExprKind::Set(xs) => {
                for x in xs {
                    self.walk_expr(x, moved);
                }
            }
            ExprKind::ListRepeat { value, count } => {
                self.walk_expr(value, moved);
                self.walk_expr(count, moved);
            }
            ExprKind::Map(entries) => {
                for (k, v) in entries {
                    self.walk_expr(k, moved);
                    self.walk_expr(v, moved);
                }
            }
            ExprKind::StructLit { fields, spread, .. } => {
                for (_, v) in fields {
                    self.walk_expr(v, moved);
                }
                if let Some(s) = spread {
                    self.walk_expr(s, moved);
                }
            }
            ExprKind::Range { start, end, .. } => {
                self.walk_expr(start, moved);
                self.walk_expr(end, moved);
            }
            ExprKind::Block(b) => self.walk_block(b, moved),
            ExprKind::Spawn(b) | ExprKind::Unsafe(b) => self.walk_block(b, moved),

            ExprKind::If { cond, then, els } => {
                self.walk_expr(cond, moved);
                let mut m_then = moved.clone();
                self.walk_block(then, &mut m_then);
                let mut m_els = moved.clone();
                if let Some(e) = els {
                    self.walk_expr(e, &mut m_els);
                }
                // A value moved in either branch is moved afterward (as in Rust).
                *moved = m_then.union(&m_els).cloned().collect();
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee, moved);
                let mut acc: Option<Moved> = None;
                for arm in arms {
                    let mut m = moved.clone();
                    bind_fresh(&arm.pattern, &mut m);
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g, &mut m);
                    }
                    self.walk_expr(&arm.body, &mut m);
                    acc = Some(match acc {
                        None => m,
                        Some(a) => a.union(&m).cloned().collect(),
                    });
                }
                if let Some(a) = acc {
                    *moved = a;
                }
            }
            ExprKind::Loop { body } => self.walk_loop(body, None, moved),
            ExprKind::While { cond, body } => {
                self.walk_expr(cond, moved);
                self.walk_loop(body, None, moved);
            }
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => {
                self.walk_expr(expr, moved);
                self.walk_loop(body, Some(pattern), moved);
            }
            ExprKind::For {
                pattern,
                iter,
                body,
            } => {
                self.walk_expr(iter, moved);
                self.walk_loop(body, Some(pattern), moved);
            }
            ExprKind::Closure {
                params,
                body,
                is_move,
            } => {
                // Reads inside the closure are checked against the current moved
                // set (so capturing an already-moved value is caught).
                let mut inner = moved.clone();
                self.walk_expr(body, &mut inner);
                // A `move` closure takes ownership of every non-Copy variable it
                // captures, so those bindings are moved once it is created.
                if *is_move {
                    for (name, id) in closure_free_vars(params, body) {
                        if !self.types.is_copy(id) {
                            moved.insert(name);
                        }
                    }
                }
            }
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                self.walk_block(body, moved);
                for c in catches {
                    let mut m = moved.clone();
                    self.walk_block(&c.body, &mut m);
                }
                if let Some(f) = finally {
                    self.walk_block(f, moved);
                }
            }
        }
    }

    /// Check a loop body. A loop repeats, so a value moved in one iteration and
    /// read in the next is a use-after-move: we discover what the body moves
    /// (first pass, errors discarded), then re-check the body as if those moves
    /// had already happened on entry (second pass, errors kept). Moves that
    /// escape the loop are unioned into the outer set.
    pub(super) fn walk_loop(&mut self, body: &Block, pattern: Option<&Pattern>, moved: &mut Moved) {
        // The state on entry to an iteration: the outer moves, with the loop's
        // own pattern bindings freshly owned.
        let mut entry = moved.clone();
        if let Some(p) = pattern {
            bind_fresh(p, &mut entry);
        }

        // First pass: discover what the body moves, discarding any diagnostics.
        let saved = self.errors.len();
        let mut probe = entry.clone();
        self.walk_block(body, &mut probe);
        self.errors.truncate(saved);

        // Second pass: re-enter with the loop-carried moves already in effect, so
        // a value moved in one iteration and read in the next is caught.
        let mut second = entry;
        for v in probe.difference(moved) {
            second.insert(v.clone());
        }
        // The loop's own pattern is re-bound every iteration, so its names are
        // never carried (a `for s in xs` body may move `s` each time).
        if let Some(p) = pattern {
            bind_fresh(p, &mut second);
        }
        self.walk_block(body, &mut second);

        // Moves that happened in the body persist after the loop.
        moved.extend(second);
    }
}

//! Borrow-region checking and escape analysis for ownership checking.

use super::support::place_of;
use super::*;

impl BorrowCk<'_> {
    // ---- borrow regions (1.6.4) -----------------------------------------

    /// Walk a block tracking live `let`-bound borrows. Borrows declared in the
    /// block end when it closes (lexical scope).
    pub(super) fn check_borrows_block(&mut self, b: &Block, active: &mut Vec<Borrow>) {
        let entry = active.len();
        for s in &b.stmts {
            self.check_borrows_stmt(s, active);
        }
        if let Some(t) = &b.tail {
            self.check_borrows_expr(t, active);
        }
        active.truncate(entry);
    }

    pub(super) fn check_borrows_stmt(&mut self, s: &Stmt, active: &mut Vec<Borrow>) {
        match s {
            Stmt::Let { pattern, value, .. } => {
                self.check_borrows_expr(value, active);
                // `let r = &x` / `let r = &mut x` registers a live borrow of `x`.
                if let Pattern::Binding(r) = pattern {
                    if let ExprKind::Unary { op, expr } = &value.kind {
                        let mutable = match op {
                            UnOp::RefMut => true,
                            UnOp::Ref => false,
                            _ => return,
                        };
                        if let Some(place) = place_of(expr) {
                            active.push(Borrow {
                                borrower: r.clone(),
                                place,
                                mutable,
                            });
                        }
                    }
                }
            }
            Stmt::Expr(e) => self.check_borrows_expr(e, active),
            Stmt::Return(Some(e), _) => {
                self.check_escape(e);
                self.check_borrows_expr(e, active);
            }
            Stmt::Break(Some(e), _) => self.check_borrows_expr(e, active),
            Stmt::Return(None, _) | Stmt::Break(None, _) | Stmt::Continue(_) => {}
            Stmt::Item(Item::Fn(f)) => self.check_fn(f),
            Stmt::Item(_) => {}
        }
    }

    /// Walk an expression, flagging any access to a place that conflicts with a
    /// live borrow. A `&place`/`&mut place` is checked as a borrow access (and
    /// not descended into, to avoid double-counting); everything else recurses.
    pub(super) fn check_borrows_expr(&mut self, e: &Expr, active: &mut Vec<Borrow>) {
        match &e.kind {
            ExprKind::Unary {
                op: op @ (UnOp::Ref | UnOp::RefMut),
                expr,
            } => {
                self.access_place_or_recurse(expr, matches!(op, UnOp::RefMut), active);
            }
            ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { .. } => {
                // A place read: check the *full* path (field-granular), and walk
                // any index sub-expressions it contains.
                self.access_place_or_recurse(e, false, active);
            }
            ExprKind::Assign { target, value, .. } => {
                self.check_borrows_expr(value, active);
                // A plain `=` writes; a compound `+=` reads then writes. Both are
                // exclusive accesses to the target place.
                self.access_place_or_recurse(target, true, active);
            }
            ExprKind::MethodCall {
                recv, method, args, ..
            } => {
                // A method that mutates its receiver (`&mut self`/`mut self`/`self`,
                // or a known mutating built-in like `push`) is an exclusive access
                // to the receiver's place; otherwise it's a shared read.
                let exclusive = self.method_mutates(recv, method);
                self.access_place_or_recurse(recv, exclusive, active);
                for a in args {
                    self.check_borrows_expr(a, active);
                }
            }
            ExprKind::Call { callee, args } => {
                self.check_borrows_expr(callee, active);
                for a in args {
                    self.check_borrows_expr(a, active);
                }
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::Try(expr)
            | ExprKind::Await(expr) => self.check_borrows_expr(expr, active),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
                self.check_borrows_expr(lhs, active);
                self.check_borrows_expr(rhs, active);
            }
            ExprKind::Tuple(xs) | ExprKind::List(xs) | ExprKind::Set(xs) => {
                xs.iter().for_each(|x| self.check_borrows_expr(x, active))
            }
            ExprKind::ListRepeat { value, count } => {
                self.check_borrows_expr(value, active);
                self.check_borrows_expr(count, active);
            }
            ExprKind::Map(entries) => entries.iter().for_each(|(k, v)| {
                self.check_borrows_expr(k, active);
                self.check_borrows_expr(v, active);
            }),
            ExprKind::StructLit { fields, spread, .. } => {
                fields
                    .iter()
                    .for_each(|(_, v)| self.check_borrows_expr(v, active));
                if let Some(s) = spread {
                    self.check_borrows_expr(s, active);
                }
            }
            ExprKind::Range { start, end, .. } => {
                self.check_borrows_expr(start, active);
                self.check_borrows_expr(end, active);
            }
            ExprKind::FStr(parts) => {
                for p in parts {
                    if let FStrPart::Expr { expr, .. } = p {
                        self.check_borrows_expr(expr, active);
                    }
                }
            }
            ExprKind::Block(b)
            | ExprKind::Loop { body: b }
            | ExprKind::Spawn(b)
            | ExprKind::Unsafe(b) => self.check_borrows_block(b, active),
            ExprKind::If { cond, then, els } => {
                self.check_borrows_expr(cond, active);
                self.check_borrows_block(then, active);
                if let Some(e) = els {
                    self.check_borrows_expr(e, active);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_borrows_expr(scrutinee, active);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.check_borrows_expr(g, active);
                    }
                    self.check_borrows_expr(&arm.body, active);
                }
            }
            ExprKind::While { cond, body } => {
                self.check_borrows_expr(cond, active);
                self.check_borrows_block(body, active);
            }
            ExprKind::WhileLet { expr, body, .. }
            | ExprKind::For {
                iter: expr, body, ..
            } => {
                self.check_borrows_expr(expr, active);
                self.check_borrows_block(body, active);
            }
            ExprKind::Closure { body, .. } => self.check_borrows_expr(body, active),
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                self.check_borrows_block(body, active);
                for c in catches {
                    self.check_borrows_block(&c.body, active);
                }
                if let Some(f) = finally {
                    self.check_borrows_block(f, active);
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
        }
    }

    /// An access to `place` (read if `!exclusive`, write/`&mut` if `exclusive`).
    /// A live `&mut` borrow forbids *any* other access; a live `&` borrow forbids
    /// exclusive accesses (writes / `&mut`). The borrow is named in the error.
    pub(super) fn check_access(
        &mut self,
        place: &Place,
        exclusive: bool,
        pos: Pos,
        active: &[Borrow],
    ) {
        if let Some(b) = active
            .iter()
            .find(|b| b.place.overlaps(place) && (b.mutable || exclusive))
        {
            let how = if b.mutable { "mutably " } else { "" };
            let act = if exclusive { "mutate" } else { "use" };
            // Name the borrowed place when it differs from the one being accessed
            // (e.g. accessing `u.age` while the whole `u` is borrowed).
            let held = if b.place == *place {
                format!("while it is {}borrowed by `{}`", how, b.borrower)
            } else {
                format!(
                    "while `{}` is {}borrowed by `{}`",
                    b.place.render(),
                    how,
                    b.borrower
                )
            };
            self.err(
                pos,
                format!(
                    "cannot {} `{}` {} (aliasing xor mutability)",
                    act,
                    place.render(),
                    held
                ),
            );
        }
    }

    /// If `e` is a place rooted in a binding, check the access (field-granular)
    /// against live borrows and walk any index sub-expressions. Otherwise `e` is
    /// a place shape over a non-place base (e.g. `foo().bar`); walk its component
    /// expressions (never `e` itself, which would re-enter this arm forever).
    pub(super) fn access_place_or_recurse(
        &mut self,
        e: &Expr,
        exclusive: bool,
        active: &mut Vec<Borrow>,
    ) {
        if let Some(p) = place_of(e) {
            self.check_place_indices(e, active);
            self.check_access(&p, exclusive, e.pos, active);
        } else {
            match &e.kind {
                ExprKind::Field { recv, .. } | ExprKind::Unary { expr: recv, .. } => {
                    self.check_borrows_expr(recv, active)
                }
                ExprKind::Index { recv, index } => {
                    self.check_borrows_expr(recv, active);
                    self.check_borrows_expr(index, active);
                }
                _ => self.check_borrows_expr(e, active),
            }
        }
    }

    /// Walk the index expressions inside a place chain (`arr[i]` reads `i`).
    pub(super) fn check_place_indices(&mut self, e: &Expr, active: &mut Vec<Borrow>) {
        match &e.kind {
            ExprKind::Field { recv, .. } => self.check_place_indices(recv, active),
            ExprKind::Unary {
                op: UnOp::Deref,
                expr,
            } => self.check_place_indices(expr, active),
            ExprKind::Index { recv, index } => {
                self.check_place_indices(recv, active);
                self.check_borrows_expr(index, active);
            }
            _ => {}
        }
    }

    /// Does calling `method` on `recv` mutate (or consume) the receiver? A user
    /// method is read-only only when it takes `&self`; a built-in counts as
    /// mutating when it is a known in-place mutator (`push`, `pop`, …). Anything
    /// else is treated as a shared read.
    pub(super) fn method_mutates(&self, recv: &Expr, method: &str) -> bool {
        if let Some(rendered) = self.types.type_of(recv.id) {
            let ty = head_name(&rendered).to_string();
            if let Some((self_kind, _)) = self.sigs.methods.get(&(ty, method.to_string())) {
                return matches!(self_kind, SelfKind::Value | SelfKind::RefMut);
            }
        }
        matches!(
            method,
            "push" | "pop" | "insert" | "remove" | "extend" | "clear" | "append" | "sort"
        )
    }

    /// Flag a value that escapes the function (a `return`/tail expression) when
    /// it is a borrow of a bare binding: `&x`/`&mut x`/`&raw x` for a local or
    /// owned parameter `x` always dangles, since `x` dies with the call frame.
    pub(super) fn check_escape(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Unary {
                op: UnOp::Ref | UnOp::RefMut | UnOp::RawRef,
                expr,
            } => {
                if let ExprKind::Ident(name) = &expr.kind {
                    self.err(
                        e.pos,
                        format!(
                            "cannot return a reference to local `{}`: it is dropped at the end \
                             of the function, so the reference would dangle",
                            name
                        ),
                    );
                }
            }
            // Recurse into tail positions of the value-producing forms.
            ExprKind::Block(b) => {
                if let Some(t) = &b.tail {
                    self.check_escape(t);
                }
            }
            ExprKind::If { then, els, .. } => {
                if let Some(t) = &then.tail {
                    self.check_escape(t);
                }
                if let Some(e) = els {
                    self.check_escape(e);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.check_escape(&arm.body);
                }
            }
            _ => {}
        }
    }
}

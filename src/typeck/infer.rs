//! Type checker: core expression inference — the `infer`/`infer_kind`
//! dispatcher, identifiers, unary/binary operators, and `??`. Split out of `typeck.rs`.

use std::collections::HashSet;

use super::*;

impl TypeChecker {
    // ---- expression inference -------------------------------------------

    /// Infer the type of an expression and record it in the type table keyed by
    /// the node's [`NodeId`]. Every expression flows through here, so the table
    /// ends up holding a concrete `Ty` for every node in the program.
    pub(super) fn infer(&mut self, e: &Expr) -> Ty {
        let ty = self.infer_kind(e);
        self.types.insert(e.id, ty.clone());
        self.type_order.push((e.pos, e.id));
        ty
    }

    pub(super) fn infer_kind(&mut self, e: &Expr) -> Ty {
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
                if any { t } else { Ty::Unit }
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

    pub(super) fn bind_pattern_refutable(&mut self, p: &Pattern, scrut: &Ty) {
        // For `if let` / `while let`, narrow `Some(x)`/`Ok(x)` to the payload.
        self.bind_pattern(p, scrut);
    }

    pub(super) fn infer_ident(&mut self, name: &str) -> Ty {
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

    pub(super) fn infer_unary(&mut self, op: UnOp, expr: &Expr, pos: Pos) -> Ty {
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

    pub(super) fn infer_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, pos: Pos) -> Ty {
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
                let offset_ok = r.is_int() || r.is_unknown() || matches!(&r, Ty::Param(_));
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
                let offset_ok = l.is_int() || l.is_unknown() || matches!(&l, Ty::Param(_));
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

    pub(super) fn infer_coalesce(&mut self, lhs: &Expr, rhs: &Expr, pos: Pos) -> Ty {
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
    pub(super) fn check_borrow_conflicts(&mut self, args: &[Expr], pos: Pos) {
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
}

//! Shared helper routines for borrow checking: place tracking, pattern binding,
//! and closure capture analysis. Split out of `borrowck.rs`.

use super::*;

/// The [`Place`] a place expression names (`x`, `x.f`, `x[i]`, `*x`), or `None`
/// if it isn't rooted in a binding (e.g. `foo().bar`).
pub(super) fn place_of(e: &Expr) -> Option<Place> {
    match &e.kind {
        ExprKind::Ident(n) => Some(Place {
            root: n.clone(),
            proj: Vec::new(),
        }),
        ExprKind::Field { recv, name, .. } => {
            let mut p = place_of(recv)?;
            p.proj.push(Proj::Field(name.clone()));
            Some(p)
        }
        ExprKind::Index { recv, .. } => {
            let mut p = place_of(recv)?;
            p.proj.push(Proj::Index);
            Some(p)
        }
        // `*r` reaches the place `r` points at; conservatively treat it as the
        // place of `r` (precise reborrow tracking is deferred to MIR).
        ExprKind::Unary {
            op: UnOp::Deref,
            expr,
        } => place_of(expr),
        _ => None,
    }
}

/// Remove from `moved` every name a pattern (re)binds — those bindings start
/// owned again. Conservative: any name introduced is cleared.
pub(super) fn bind_fresh(p: &Pattern, moved: &mut Moved) {
    match p {
        Pattern::Binding(n) | Pattern::Typed { binding: n, .. } => {
            moved.remove(n);
        }
        Pattern::At(n, sub) => {
            moved.remove(n);
            bind_fresh(sub, moved);
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => {
            for p in ps {
                bind_fresh(p, moved);
            }
        }
        Pattern::List { items, rest } => {
            for p in items {
                bind_fresh(p, moved);
            }
            if let Some(r) = rest {
                moved.remove(r);
            }
        }
        Pattern::Variant { args, .. } => {
            for p in args {
                bind_fresh(p, moved);
            }
        }
        Pattern::Struct { fields, .. } => {
            for f in fields {
                moved.remove(f);
            }
        }
        Pattern::Wildcard
        | Pattern::Int(_)
        | Pattern::Str(_)
        | Pattern::Bool(_)
        | Pattern::Char(_)
        | Pattern::Nil
        | Pattern::Range { .. } => {}
    }
}

/// Insert every name a pattern binds into `set`.
pub(super) fn add_pattern_names(p: &Pattern, set: &mut HashSet<String>) {
    match p {
        Pattern::Binding(n) | Pattern::Typed { binding: n, .. } => {
            set.insert(n.clone());
        }
        Pattern::At(n, sub) => {
            set.insert(n.clone());
            add_pattern_names(sub, set);
        }
        Pattern::Tuple(ps) | Pattern::Or(ps) => ps.iter().for_each(|p| add_pattern_names(p, set)),
        Pattern::List { items, rest } => {
            items.iter().for_each(|p| add_pattern_names(p, set));
            if let Some(r) = rest {
                set.insert(r.clone());
            }
        }
        Pattern::Variant { args, .. } => args.iter().for_each(|p| add_pattern_names(p, set)),
        Pattern::Struct { fields, .. } => fields.iter().for_each(|f| {
            set.insert(f.clone());
        }),
        Pattern::Wildcard
        | Pattern::Int(_)
        | Pattern::Str(_)
        | Pattern::Bool(_)
        | Pattern::Char(_)
        | Pattern::Nil
        | Pattern::Range { .. } => {}
    }
}

/// The free variables a `move` closure captures: identifiers referenced in the
/// body that are not bound by the closure's parameters or by any `let`/pattern
/// inside it. Returns one `(name, node id)` per distinct name (the id lets the
/// caller query the captured value's type). Over-subtracting bound names keeps
/// this conservative — it never reports a capture that isn't one.
pub(super) fn closure_free_vars(params: &[Param], body: &Expr) -> Vec<(String, NodeId)> {
    let mut idents: Vec<(String, NodeId)> = Vec::new();
    let mut bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    collect_refs(body, &mut idents, &mut bound);
    let mut seen = HashSet::new();
    idents
        .into_iter()
        .filter(|(n, _)| !bound.contains(n) && seen.insert(n.clone()))
        .collect()
}

/// Collect every identifier reference (into `idents`) and every locally-bound
/// name (into `bound`) within an expression. Used only by `closure_free_vars`.
fn collect_refs(e: &Expr, idents: &mut Vec<(String, NodeId)>, bound: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Ident(n) => idents.push((n.clone(), e.id)),
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
                    collect_refs(expr, idents, bound);
                }
            }
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::Try(expr)
        | ExprKind::Await(expr)
        | ExprKind::Field { recv: expr, .. } => collect_refs(expr, idents, bound),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
            collect_refs(lhs, idents, bound);
            collect_refs(rhs, idents, bound);
        }
        ExprKind::Assign { target, value, .. } => {
            collect_refs(target, idents, bound);
            collect_refs(value, idents, bound);
        }
        ExprKind::Call { callee, args } => {
            collect_refs(callee, idents, bound);
            args.iter().for_each(|a| collect_refs(a, idents, bound));
        }
        ExprKind::MethodCall { recv, args, .. } => {
            collect_refs(recv, idents, bound);
            args.iter().for_each(|a| collect_refs(a, idents, bound));
        }
        ExprKind::Index { recv, index } => {
            collect_refs(recv, idents, bound);
            collect_refs(index, idents, bound);
        }
        ExprKind::Tuple(xs) | ExprKind::List(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| collect_refs(x, idents, bound))
        }
        ExprKind::ListRepeat { value, count } => {
            collect_refs(value, idents, bound);
            collect_refs(count, idents, bound);
        }
        ExprKind::Map(entries) => entries.iter().for_each(|(k, v)| {
            collect_refs(k, idents, bound);
            collect_refs(v, idents, bound);
        }),
        ExprKind::StructLit { fields, spread, .. } => {
            fields
                .iter()
                .for_each(|(_, v)| collect_refs(v, idents, bound));
            if let Some(s) = spread {
                collect_refs(s, idents, bound);
            }
        }
        ExprKind::Range { start, end, .. } => {
            collect_refs(start, idents, bound);
            collect_refs(end, idents, bound);
        }
        ExprKind::Block(b)
        | ExprKind::Loop { body: b }
        | ExprKind::Spawn(b)
        | ExprKind::Unsafe(b) => collect_block_refs(b, idents, bound),
        ExprKind::If { cond, then, els } => {
            collect_refs(cond, idents, bound);
            collect_block_refs(then, idents, bound);
            if let Some(e) = els {
                collect_refs(e, idents, bound);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_refs(scrutinee, idents, bound);
            for arm in arms {
                add_pattern_names(&arm.pattern, bound);
                if let Some(g) = &arm.guard {
                    collect_refs(g, idents, bound);
                }
                collect_refs(&arm.body, idents, bound);
            }
        }
        ExprKind::While { cond, body } => {
            collect_refs(cond, idents, bound);
            collect_block_refs(body, idents, bound);
        }
        ExprKind::WhileLet {
            pattern,
            expr,
            body,
        }
        | ExprKind::For {
            pattern,
            iter: expr,
            body,
        } => {
            collect_refs(expr, idents, bound);
            add_pattern_names(pattern, bound);
            collect_block_refs(body, idents, bound);
        }
        ExprKind::Closure { params, body, .. } => {
            for p in params {
                bound.insert(p.name.clone());
            }
            collect_refs(body, idents, bound);
        }
        ExprKind::TryCatch {
            body,
            catches,
            finally,
        } => {
            collect_block_refs(body, idents, bound);
            for c in catches {
                if let Some(b) = &c.binding {
                    bound.insert(b.clone());
                }
                collect_block_refs(&c.body, idents, bound);
            }
            if let Some(f) = finally {
                collect_block_refs(f, idents, bound);
            }
        }
    }
}

fn collect_block_refs(b: &Block, idents: &mut Vec<(String, NodeId)>, bound: &mut HashSet<String>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { pattern, value, .. } => {
                collect_refs(value, idents, bound);
                add_pattern_names(pattern, bound);
            }
            Stmt::Expr(e) => collect_refs(e, idents, bound),
            Stmt::Return(Some(e), _) | Stmt::Break(Some(e), _) => collect_refs(e, idents, bound),
            Stmt::Return(None, _) | Stmt::Break(None, _) | Stmt::Continue(_) | Stmt::Item(_) => {}
        }
    }
    if let Some(t) = &b.tail {
        collect_refs(t, idents, bound);
    }
}

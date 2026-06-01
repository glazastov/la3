//! Ownership / borrow checking (Phase 1.6).
//!
//! La3 has Rust-style ownership (reference Section 11). This pass runs after a
//! clean type check (so it has a reliable [`TypeTable`]) and enforces the parts
//! of that model the compiler back-end will rely on for deterministic drop.
//!
//! **Move semantics + use-after-move (1.6.1–1.6.2).** A *move* transfers
//! ownership out of a binding; using the binding afterward is an error unless
//! the type is [`Copy`](TypeTable::is_copy). Moves happen at:
//! - `let y = x` / `x = y` — whole-binding moves (1.6.1);
//! - **by-value arguments** to a user function/method — `f(x)` moves `x` when the
//!   matching parameter is taken by value (not `&T`/`&[T]`/`*T`) (1.6.2);
//! - **consuming receivers** — `x.m()` moves `x` when `m` takes `self`/`mut self`
//!   (1.6.2).
//!
//! `&x`/`&mut x` are borrows, never moves. Calls to the built-in stdlib borrow
//! their arguments and receiver (their signatures aren't user-declared, and the
//! examples reuse values after passing them to `io.println`, `to_hex`, `.map`,
//! `.get`, …), so only **user-declared** functions/methods move. `move`-closure
//! captures and `&mut` exclusivity are still to come.
//!
//! The analysis is flow-sensitive: it threads a set of moved-out bindings through
//! straight-line code, takes the **union** across `if`/`match` branches (a value
//! moved in any branch is moved afterward, as in Rust), and checks loop bodies
//! twice so a value moved in one iteration and used in the next is caught. A
//! later `let`/`=` re-initializes a binding and clears its moved mark.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos};
use crate::typeck::TypeTable;

/// Run the borrow checker over a program. Returns ownership diagnostics (the
/// caller has already confirmed names and types resolve).
pub fn check(prog: &Program, types: &TypeTable) -> Vec<Diagnostic> {
    let mut bc = BorrowCk {
        types,
        sigs: collect_sigs(prog),
        errors: Vec::new(),
    };
    for item in &prog.items {
        match item {
            Item::Fn(f) => bc.check_fn(f),
            Item::Impl(b) => {
                for m in &b.methods {
                    bc.check_fn(m);
                }
            }
            Item::Const(c) => {
                let mut moved = Moved::new();
                bc.walk_expr(&c.value, &mut moved);
            }
            _ => {}
        }
    }
    bc.errors.sort_by_key(|d| (d.pos.line, d.pos.col));
    bc.errors
}

/// The set of bindings that have been moved out and not since re-initialized.
type Moved = HashSet<String>;

/// Call signatures collected from the program, so a call site can tell which
/// arguments/receivers are taken by value (a move) versus borrowed. Only
/// user-declared functions and methods appear here; anything absent is a
/// built-in and borrows.
struct Sigs {
    /// Free function name → per-parameter "taken by value" flags.
    free_fns: HashMap<String, Vec<bool>>,
    /// (type, method) → (receiver form, per-parameter "by value" flags).
    methods: HashMap<(String, String), (SelfKind, Vec<bool>)>,
    /// Declared struct/enum names, to spot type-qualified calls (`Type.assoc()`).
    type_names: HashSet<String>,
}

/// Is a parameter of this type taken by value (so a bare-binding argument is
/// moved)? References, slices, and raw pointers borrow; everything else owns.
fn is_by_value_ty(t: &TypeExpr) -> bool {
    !matches!(
        t,
        TypeExpr::Ref { .. } | TypeExpr::Slice(_) | TypeExpr::Ptr { .. }
    )
}

/// The by-value flags for a function's non-`self` parameters. An untyped
/// parameter is treated as borrowing (lenient — never invent a move).
fn by_value_params(f: &FnDecl) -> Vec<bool> {
    f.params
        .iter()
        .filter(|p| !p.is_self)
        .map(|p| p.ty.as_ref().is_some_and(is_by_value_ty))
        .collect()
}

fn collect_sigs(prog: &Program) -> Sigs {
    let mut free_fns = HashMap::new();
    let mut methods = HashMap::new();
    let mut type_names = HashSet::new();
    for item in &prog.items {
        match item {
            Item::Fn(f) => {
                free_fns.insert(f.name.clone(), by_value_params(f));
            }
            Item::Impl(b) => {
                for m in &b.methods {
                    methods.insert(
                        (b.ty.clone(), m.name.clone()),
                        (m.self_kind, by_value_params(m)),
                    );
                }
            }
            Item::Struct(s) => {
                type_names.insert(s.name.clone());
            }
            Item::Enum(e) => {
                type_names.insert(e.name.clone());
            }
            _ => {}
        }
    }
    Sigs {
        free_fns,
        methods,
        type_names,
    }
}

/// The nominal head of a rendered type (`List<i32>` → `List`, `Point` → `Point`).
fn head_name(rendered: &str) -> &str {
    rendered.split('<').next().unwrap_or(rendered)
}

struct BorrowCk<'a> {
    types: &'a TypeTable,
    sigs: Sigs,
    errors: Vec<Diagnostic>,
}

impl BorrowCk<'_> {
    fn err(&mut self, pos: Pos, msg: impl Into<String>) {
        self.errors.push(Diagnostic::new(Phase::Check, pos, msg));
    }

    fn check_fn(&mut self, f: &FnDecl) {
        let mut moved = Moved::new();
        self.walk_block(&f.body, &mut moved);
    }

    // ---- statements ------------------------------------------------------

    fn walk_block(&mut self, b: &Block, moved: &mut Moved) {
        for s in &b.stmts {
            self.walk_stmt(s, moved);
        }
        if let Some(t) = &b.tail {
            self.walk_expr(t, moved);
        }
    }

    fn walk_stmt(&mut self, s: &Stmt, moved: &mut Moved) {
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
    fn try_move(&mut self, e: &Expr, moved: &mut Moved) {
        if let ExprKind::Ident(name) = &e.kind {
            if !self.types.is_copy(e.id) {
                moved.insert(name.clone());
            }
        }
    }

    /// Move each argument whose matching parameter is taken by value.
    fn move_by_value_args(&mut self, args: &[Expr], by_value: &[bool], moved: &mut Moved) {
        for (a, &by_val) in args.iter().zip(by_value) {
            if by_val {
                self.try_move(a, moved);
            }
        }
    }

    // ---- expressions -----------------------------------------------------

    fn walk_expr(&mut self, e: &Expr, moved: &mut Moved) {
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
            ExprKind::Closure { body, .. } => {
                // 1.6.1 only checks reads inside the closure against the current
                // moved set (so a capture of an already-moved value is caught).
                // Capture-by-move is tracked in 1.6.2.
                let mut inner = moved.clone();
                self.walk_expr(body, &mut inner);
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
    fn walk_loop(&mut self, body: &Block, pattern: Option<&Pattern>, moved: &mut Moved) {
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

/// Remove from `moved` every name a pattern (re)binds — those bindings start
/// owned again. Conservative: any name introduced is cleared.
fn bind_fresh(p: &Pattern, moved: &mut Moved) {
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

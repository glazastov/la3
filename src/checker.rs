//! The checker runs in two passes.
//!
//! 1. **Name resolution** (this module): reports references to names that are
//!    not defined anywhere in scope (locals, parameters, functions, consts,
//!    types, enum variants, the standard modules, or the builtins).
//! 2. **Type checking** ([`crate::typeck`]): enforces the typing rules from
//!    reference Sections 2, 4, 7, and 9.
//!
//! Name resolution runs first so the type pass can assume that every referenced
//! name exists; type diagnostics are appended and the combined list is sorted by
//! source position.

use std::collections::HashSet;

use crate::ast::*;
use crate::diag::{Diagnostic, Phase};

pub fn check(prog: &Program) -> Vec<Diagnostic> {
    let mut r = Resolver::new(prog);
    r.run(prog);
    let mut errors = r.errors;
    // Only run the type pass when names all resolve, so undefined-name noise
    // does not produce confusing downstream type errors.
    if errors.is_empty() {
        let table = crate::typeck::check_types(prog);
        if table.errors.is_empty() {
            // Ownership/borrow checking needs a reliable type table, so it only
            // runs once names and types are clean (reference Section 11).
            errors.extend(crate::borrowck::check(prog, &table));
        } else {
            errors.extend(table.errors);
        }
    }
    errors.sort_by_key(|d| (d.pos.line, d.pos.col));
    errors
}

struct Resolver {
    globals: HashSet<String>,
    scopes: Vec<HashSet<String>>,
    errors: Vec<Diagnostic>,
}

fn builtins() -> HashSet<String> {
    [
        // free builtins
        "str", "len", "print", "println", "min", "max", "abs", "idiv", "all", "race", "to_hex",
        "from_hex", // enum constructors
        "Some", "None", "Ok", "Err", // modules
        "io", "fs", "net", "http", "dns", "tcp", "bytes", "crypto", "json", "os", "math",
        // common library types
        "Option", "Result", "List", "Map", "Set", "Vec", "Self", "self",
        // primitives used as cast targets / constructors
        "channel", "spawn", "alloc", "dealloc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Resolver {
    fn new(prog: &Program) -> Self {
        let mut globals = builtins();
        for item in &prog.items {
            match item {
                Item::Fn(f) => {
                    globals.insert(f.name.clone());
                }
                Item::Struct(s) => {
                    globals.insert(s.name.clone());
                }
                Item::Enum(e) => {
                    globals.insert(e.name.clone());
                    for v in &e.variants {
                        globals.insert(v.name.clone());
                    }
                }
                Item::Const(c) => {
                    globals.insert(c.name.clone());
                }
                Item::TypeAlias { name, .. } => {
                    globals.insert(name.clone());
                }
                Item::Interface(i) => {
                    globals.insert(i.name.clone());
                }
                Item::Impl(_) | Item::Use(_) => {}
            }
        }
        Resolver {
            globals,
            scopes: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn run(&mut self, prog: &Program) {
        for item in &prog.items {
            match item {
                Item::Fn(f) => self.fn_decl(f),
                Item::Impl(b) => {
                    for m in &b.methods {
                        self.fn_decl(m);
                    }
                }
                Item::Const(c) => self.expr(&c.value),
                _ => {}
            }
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashSet::new());
    }
    fn pop(&mut self) {
        self.scopes.pop();
    }
    fn declare(&mut self, name: &str) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.to_string());
        } else {
            self.globals.insert(name.to_string());
        }
    }
    fn known(&self, name: &str) -> bool {
        name == "_" || self.globals.contains(name) || self.scopes.iter().any(|s| s.contains(name))
    }

    fn fn_decl(&mut self, f: &FnDecl) {
        self.push();
        for (g, _) in &f.generics {
            self.declare(g);
        }
        for p in &f.params {
            self.declare(if p.is_self { "self" } else { &p.name });
        }
        if let Some(v) = &f.variadic {
            self.declare(&v.name);
        }
        self.block(&f.body);
        self.pop();
    }

    fn block(&mut self, b: &Block) {
        self.push();
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
        self.pop();
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Let { pattern, value, .. } => {
                self.expr(value);
                self.bind_pattern(pattern);
            }
            Stmt::Expr(e) => self.expr(e),
            Stmt::Return(Some(e), _) | Stmt::Break(Some(e), _) => self.expr(e),
            Stmt::Return(None, _) | Stmt::Break(None, _) | Stmt::Continue(_) => {}
            Stmt::Item(item) => match item {
                Item::Fn(f) => {
                    self.declare(&f.name);
                    self.fn_decl(f);
                }
                Item::Const(c) => {
                    self.expr(&c.value);
                    self.declare(&c.name);
                }
                _ => {}
            },
        }
    }

    fn bind_pattern(&mut self, p: &Pattern) {
        match p {
            Pattern::Binding(n) => self.declare(n),
            Pattern::At(n, sub) => {
                self.declare(n);
                self.bind_pattern(sub);
            }
            Pattern::Tuple(ps) | Pattern::Or(ps) => ps.iter().for_each(|p| self.bind_pattern(p)),
            Pattern::List { items, rest } => {
                items.iter().for_each(|p| self.bind_pattern(p));
                if let Some(r) = rest {
                    if !r.is_empty() {
                        self.declare(r);
                    }
                }
            }
            Pattern::Variant { args, .. } => args.iter().for_each(|p| self.bind_pattern(p)),
            Pattern::Struct { fields, .. } => fields.iter().for_each(|f| self.declare(f)),
            Pattern::Typed { binding, .. } => self.declare(binding),
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(name) => {
                if !self.known(name) {
                    self.errors.push(Diagnostic::new(
                        Phase::Check,
                        e.pos,
                        format!("undefined name '{}'", name),
                    ));
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
                        self.expr(expr);
                    }
                }
            }
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            ExprKind::Cast { expr, .. } => self.expr(expr),
            ExprKind::Call { callee, args } => {
                // A bare callee that names a type/module/variant is fine.
                self.expr(callee);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::MethodCall { recv, args, .. } => {
                self.expr(recv);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Field { recv, .. } => self.expr(recv),
            ExprKind::Index { recv, index } => {
                self.expr(recv);
                self.expr(index);
            }
            ExprKind::Tuple(xs) | ExprKind::List(xs) | ExprKind::Set(xs) => {
                xs.iter().for_each(|x| self.expr(x));
            }
            ExprKind::ListRepeat { value, count } => {
                self.expr(value);
                self.expr(count);
            }
            ExprKind::Map(entries) => {
                for (k, v) in entries {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::StructLit { fields, spread, .. } => {
                for (_, v) in fields {
                    self.expr(v);
                }
                if let Some(s) = spread {
                    self.expr(s);
                }
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::If { cond, then, els } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = els {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.push();
                    self.bind_pattern(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                    self.pop();
                }
            }
            ExprKind::Loop { body } | ExprKind::Spawn(body) | ExprKind::Unsafe(body) => {
                self.block(body)
            }
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => {
                self.expr(expr);
                self.push();
                self.bind_pattern(pattern);
                self.block(body);
                self.pop();
            }
            ExprKind::For {
                pattern,
                iter,
                body,
            } => {
                self.expr(iter);
                self.push();
                self.bind_pattern(pattern);
                self.block(body);
                self.pop();
            }
            ExprKind::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
            ExprKind::Closure { params, body, .. } => {
                self.push();
                for p in params {
                    self.declare(&p.name);
                }
                self.expr(body);
                self.pop();
            }
            ExprKind::Try(e) | ExprKind::Await(e) => self.expr(e),
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                self.block(body);
                for c in catches {
                    self.push();
                    if let Some(b) = &c.binding {
                        self.declare(b);
                    }
                    self.block(&c.body);
                    self.pop();
                }
                if let Some(f) = finally {
                    self.block(f);
                }
            }
        }
    }
}

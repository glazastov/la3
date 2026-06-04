//! The checker runs in two passes.
//!
//! 1. **Name resolution** (this module): reports references to names that are
//!    not defined anywhere in scope (locals, parameters, functions, consts,
//!    types, enum variants, the standard modules, or the builtins), and assigns
//!    a unique [`BindingId`] to every *value binding site* (a `let`, parameter,
//!    pattern binding, closure param, …), mapping each *use* of a local to its
//!    binding. Shadowing is resolved here, once — downstream passes (HIR/MIR)
//!    work on ids and never reason about names again.
//! 2. **Type checking** ([`crate::typeck`]): enforces the typing rules from
//!    reference Sections 2, 4, 7, and 9.
//!
//! Name resolution runs first so the type pass can assume that every referenced
//! name exists; type diagnostics are appended and the combined list is sorted by
//! source position.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos};

/// The product of name resolution: the binding each local *use* refers to, the
/// name of every binding (indexed by [`BindingId`]), and any diagnostics.
pub struct Resolutions {
    /// Use-site (`Ident`/`self`) `NodeId` → the local binding it refers to.
    /// Absent ⇒ the use named a global item or builtin (resolved by name).
    uses: HashMap<NodeId, BindingId>,
    /// Binding name, indexed by `BindingId.0`.
    bindings: Vec<String>,
    /// `(position, binding)` per use, in resolution order, for the `la3 resolve`
    /// dump.
    use_sites: Vec<(Pos, BindingId)>,
    pub errors: Vec<Diagnostic>,
}

impl Resolutions {
    /// The local binding a use-site node refers to, or `None` if it named a
    /// global/builtin.
    #[allow(dead_code)]
    pub fn binding_of(&self, use_site: NodeId) -> Option<BindingId> {
        self.uses.get(&use_site).copied()
    }

    /// The source name of a binding.
    #[allow(dead_code)]
    pub fn name(&self, b: BindingId) -> &str {
        &self.bindings[b.0 as usize]
    }

    /// How many binding ids were allocated. HIR desugaring (Phase 2.4) needs
    /// fresh ids for the temporaries it introduces (`??`/`?.`/`?` matches); it
    /// starts them at this count so they never collide with a real binding.
    #[allow(dead_code)]
    pub fn binding_count(&self) -> u32 {
        self.bindings.len() as u32
    }

    /// Debug view for the `la3 resolve` command: every binding, then every use
    /// resolved to its binding (sorted by position).
    pub fn dump(&self) -> String {
        let mut out = String::from("bindings:\n");
        for (i, name) in self.bindings.iter().enumerate() {
            out.push_str(&format!("  #{:<3} {}\n", i, name));
        }
        out.push_str("uses:\n");
        let mut sites = self.use_sites.clone();
        sites.sort_by_key(|(p, _)| (p.line, p.col));
        for (pos, b) in sites {
            out.push_str(&format!(
                "  {:>4}:{:<3} {} -> #{}\n",
                pos.line,
                pos.col,
                self.bindings[b.0 as usize],
                b.0
            ));
        }
        out
    }
}

/// Run name resolution over a program.
pub fn resolve(prog: &Program) -> Resolutions {
    let mut r = Resolver::new(prog);
    r.run(prog);
    Resolutions {
        uses: r.uses,
        bindings: r.bindings,
        use_sites: r.use_sites,
        errors: r.errors,
    }
}

pub fn check(prog: &Program) -> Vec<Diagnostic> {
    let res = resolve(prog);
    let mut errors = res.errors;
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
    /// Global items + builtins, resolved by name (no `BindingId`).
    globals: HashSet<String>,
    /// Lexical scopes of *local* value bindings, innermost last.
    scopes: Vec<HashMap<String, BindingId>>,
    next: u32,
    uses: HashMap<NodeId, BindingId>,
    bindings: Vec<String>,
    use_sites: Vec<(Pos, BindingId)>,
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
            next: 0,
            uses: HashMap::new(),
            bindings: Vec::new(),
            use_sites: Vec::new(),
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
        self.scopes.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.scopes.pop();
    }

    /// Introduce a fresh local binding in the innermost scope and return its id.
    /// A later `let` of the same name shadows the earlier one (its own id).
    fn declare(&mut self, name: &str) -> BindingId {
        let id = BindingId(self.next);
        self.next += 1;
        self.bindings.push(name.to_string());
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.to_string(), id);
        }
        id
    }

    /// The binding a local name resolves to, searching innermost scope outward.
    fn lookup(&self, name: &str) -> Option<BindingId> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    /// Record (and validate) a use of `name` at `pos`/`node`. A local use is
    /// recorded against its binding; a global/builtin is accepted by name; an
    /// unknown name is an error.
    fn use_name(&mut self, name: &str, pos: Pos, node: NodeId) {
        if name == "_" {
            return;
        }
        if let Some(id) = self.lookup(name) {
            self.uses.insert(node, id);
            self.use_sites.push((pos, id));
        } else if !self.globals.contains(name) {
            self.errors.push(Diagnostic::new(
                Phase::Check,
                pos,
                format!("undefined name '{}'", name),
            ));
        }
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
            Pattern::Binding(n) => {
                self.declare(n);
            }
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
            Pattern::Struct { fields, .. } => fields.iter().for_each(|f| {
                self.declare(f);
            }),
            Pattern::Typed { binding, .. } => {
                self.declare(binding);
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(name) => self.use_name(name, e.pos, e.id),
            ExprKind::SelfExpr => self.use_name("self", e.pos, e.id),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::Nil
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

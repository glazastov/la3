//! Type checker: blocks, statements, and irrefutable pattern binding
//! (`let`, destructuring). Split out of `typeck.rs`.

use super::*;

impl TypeChecker {
    // ---- statements ------------------------------------------------------

    pub(super) fn check_block(&mut self, b: &Block) -> Ty {
        self.push_scope();
        for s in &b.stmts {
            self.check_stmt(s);
        }
        let ty = match &b.tail {
            Some(e) => self.infer(e),
            None => Ty::Unit,
        };
        self.pop_scope();
        ty
    }

    pub(super) fn check_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Let {
                pattern,
                ty,
                value,
                pos,
                ..
            } => {
                let declared = ty.as_ref().map(|t| self.resolve(t));
                let mut got = self.infer(value);
                if let Some(d) = &declared {
                    self.expect(&got, d, *pos, "let binding");
                    self.pin_literals(value, d);
                    got = d.clone();
                }
                self.bind_pattern(pattern, &got);
            }
            Stmt::Expr(e) => {
                self.infer(e);
            }
            Stmt::Return(opt, pos) => {
                let ret = self.ret_stack.last().cloned().unwrap_or(Ty::Unit);
                let got = match opt {
                    Some(e) => self.infer(e),
                    None => Ty::Unit,
                };
                self.expect(&got, &ret, *pos, "return value");
                if let Some(e) = opt {
                    self.pin_literals(e, &ret);
                }
            }
            Stmt::Break(opt, _) => {
                let got = match opt {
                    Some(e) => self.infer(e),
                    None => Ty::Unit,
                };
                if let Some(breaks) = self.loop_breaks.last_mut() {
                    breaks.push(got);
                }
            }
            Stmt::Continue(_) => {}
            Stmt::Item(item) => {
                if let Item::Fn(f) = item {
                    let sig = self.fn_sig(f, &self.type_params.clone());
                    self.fns.insert(f.name.clone(), sig);
                    self.check_fn(f, &self.type_params.clone());
                }
            }
        }
    }

    /// Bind the names introduced by an irrefutable pattern, threading the known
    /// type of the matched value into each binding.
    pub(super) fn bind_pattern(&mut self, p: &Pattern, ty: &Ty) {
        match p {
            Pattern::Binding(n) => self.declare(n, ty.clone()),
            Pattern::At(n, sub) => {
                self.declare(n, ty.clone());
                self.bind_pattern(sub, ty);
            }
            Pattern::Tuple(ps) => {
                let elems = match ty {
                    Ty::Tuple(ts) if ts.len() == ps.len() => ts.clone(),
                    _ => vec![Ty::Unknown; ps.len()],
                };
                for (p, t) in ps.iter().zip(elems.iter()) {
                    self.bind_pattern(p, t);
                }
            }
            Pattern::List { items, rest } => {
                let elem = elem_ty(ty);
                for p in items {
                    self.bind_pattern(p, &elem);
                }
                if let Some(r) = rest {
                    if !r.is_empty() {
                        self.declare(r, Ty::List(Box::new(elem)));
                    }
                }
            }
            Pattern::Variant { path, args } => {
                let payloads = self.variant_payloads(path, ty, args.len());
                for (p, t) in args.iter().zip(payloads.iter()) {
                    self.bind_pattern(p, t);
                }
            }
            Pattern::Struct { name, fields } => {
                for f in fields {
                    let fty = self
                        .field_ty(&Ty::Struct(name.clone(), vec![]), f)
                        .unwrap_or(Ty::Unknown);
                    self.declare(f, fty);
                }
            }
            Pattern::Typed { binding, ty: te } => {
                let narrowed = self.resolve(te);
                self.declare(binding, narrowed);
            }
            Pattern::Or(ps) => {
                for p in ps {
                    self.bind_pattern(p, ty);
                }
            }
            _ => {}
        }
    }

    /// The payload types of an enum variant pattern. `Option`/`Result` are known
    /// exactly; user enums keep payloads as `Unknown` (the AST records arity but
    /// not the element types of a variant).
    pub(super) fn variant_payloads(&self, path: &[String], scrut: &Ty, arity: usize) -> Vec<Ty> {
        let variant = path.last().map(|s| s.as_str()).unwrap_or("");
        let inner = match scrut {
            Ty::Enum(_, args) => args.first().cloned().unwrap_or(Ty::Unknown),
            _ => Ty::Unknown,
        };
        match variant {
            "Some" | "Ok" => vec![inner],
            "Err" => vec![Ty::Str],
            "None" => vec![],
            _ => vec![Ty::Unknown; arity],
        }
    }
}

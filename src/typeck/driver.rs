//! Type checker: the top-level driver (`run`), function checking, lexical
//! scopes, and the diagnostic/expectation helpers. Split out of `typeck.rs`.

use std::collections::{HashMap, HashSet};

use super::*;

impl TypeChecker {
    // ---- driver ----------------------------------------------------------

    pub(super) fn run(&mut self, prog: &Program) {
        for item in &prog.items {
            match item {
                Item::Fn(f) => self.check_fn(f, &HashSet::new()),
                Item::Impl(b) => {
                    let owner_generics: HashSet<String> = self
                        .structs
                        .get(&b.ty)
                        .map(|s| s.generics.clone())
                        .or_else(|| self.enums.get(&b.ty).map(|e| e.generics.clone()))
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    let self_ty = self.named_self_ty(&b.ty);
                    for m in &b.methods {
                        self.check_fn_with_self(m, &owner_generics, Some(self_ty.clone()));
                    }
                    if let Some(iface) = &b.interface {
                        self.check_conformance(b, iface);
                    }
                }
                Item::Const(c) => {
                    let declared = c.ty.as_ref().map(|t| self.resolve(t));
                    let got = self.infer(&c.value);
                    if let Some(d) = &declared {
                        self.expect(&got, d, c.pos, "const initializer");
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn named_self_ty(&self, name: &str) -> Ty {
        if self.enums.contains_key(name) {
            Ty::Enum(name.to_string(), vec![])
        } else {
            Ty::Struct(name.to_string(), vec![])
        }
    }

    pub(super) fn check_fn(&mut self, f: &FnDecl, extra_generics: &HashSet<String>) {
        self.check_fn_with_self(f, extra_generics, None);
    }

    pub(super) fn check_fn_with_self(
        &mut self,
        f: &FnDecl,
        extra_generics: &HashSet<String>,
        self_ty: Option<Ty>,
    ) {
        let saved = self.type_params.clone();
        for g in extra_generics {
            self.type_params.insert(g.clone());
        }
        for (g, _) in &f.generics {
            self.type_params.insert(g.clone());
        }
        self.push_scope();
        if let Some(st) = &self_ty {
            self.declare("self", st.clone());
        }
        for p in &f.params {
            if p.is_self {
                if let Some(st) = &self_ty {
                    self.declare("self", st.clone());
                }
            } else {
                let ty =
                    p.ty.as_ref()
                        .map(|t| self.resolve(t))
                        .unwrap_or(Ty::Unknown);
                self.declare(&p.name, ty);
            }
        }
        if let Some(v) = &f.variadic {
            let elem =
                v.ty.as_ref()
                    .map(|t| self.resolve(t))
                    .unwrap_or(Ty::Unknown);
            self.declare(&v.name, Ty::List(Box::new(elem)));
        }
        let mut ret = f.ret.as_ref().map(|t| self.resolve(t)).unwrap_or(Ty::Unit);
        if f.is_async {
            ret = ret.strip_future();
        }
        self.ret_stack.push(ret.clone());
        let body_ty = self.check_block(&f.body);
        // The trailing expression is an implicit return.
        if !matches!(ret, Ty::Unit) {
            self.expect(&body_ty, &ret, f.body.pos, "function return value");
        }
        self.ret_stack.pop();
        self.pop_scope();
        self.type_params = saved;
    }

    // ---- scopes ----------------------------------------------------------

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    pub(super) fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    pub(super) fn declare(&mut self, name: &str, ty: Ty) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.to_string(), ty);
        }
    }
    pub(super) fn lookup(&self, name: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            if let Some(t) = s.get(name) {
                return Some(t.clone());
            }
        }
        if let Some(t) = self.consts.get(name) {
            return Some(t.clone());
        }
        None
    }

    pub(super) fn err(&mut self, pos: Pos, msg: impl Into<String>) {
        self.errors
            .push(Diagnostic::new(Phase::Check, pos, msg.into()));
    }

    /// Report when `got` is not assignable to `want`. Lenient by design: an
    /// `Unknown`, `Param`, or numeric-literal type never triggers a complaint.
    pub(super) fn expect(&mut self, got: &Ty, want: &Ty, pos: Pos, ctx: &str) {
        if !assignable(got, want) {
            self.err(
                pos,
                format!(
                    "type mismatch in {}: expected `{}`, found `{}`",
                    ctx,
                    display_ty(want),
                    display_ty(got)
                ),
            );
        }
    }
}

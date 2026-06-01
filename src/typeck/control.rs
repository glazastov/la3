//! Type checker: `if`/`match` inference, exhaustiveness, closures, `try`,
//! and impl-conformance checking. Split out of `typeck.rs`.

use std::collections::HashSet;

use super::*;

impl TypeChecker {
    pub(super) fn infer_if(
        &mut self,
        cond: &Expr,
        then: &Block,
        els: &Option<Box<Expr>>,
        _pos: Pos,
    ) -> Ty {
        let c = self.infer(cond);
        self.expect_bool(&c, cond.pos, "if condition");
        let then_ty = self.check_block(then);
        match els {
            Some(e) => {
                let else_ty = self.infer(e);
                if !unifies(&then_ty, &else_ty) {
                    self.err(
                        then.pos,
                        format!(
                            "`if` and `else` branches have incompatible types: `{}` and `{}`",
                            display_ty(&then_ty),
                            display_ty(&else_ty)
                        ),
                    );
                }
                join(&then_ty, &else_ty)
            }
            // An `if` without `else` is a statement; its value is unit.
            None => Ty::Unit,
        }
    }

    pub(super) fn infer_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], pos: Pos) -> Ty {
        let scrut = self.infer(scrutinee);
        let mut result = Ty::Unknown;
        let mut first = true;
        let mut prev: Option<Ty> = None;
        for arm in arms {
            self.push_scope();
            self.bind_pattern(&arm.pattern, &scrut);
            if let Some(g) = &arm.guard {
                let gt = self.infer(g);
                self.expect_bool(&gt, g.pos, "match guard");
            }
            let arm_ty = self.infer(&arm.body);
            self.pop_scope();
            if !first {
                if let Some(p) = &prev {
                    if !unifies(p, &arm_ty) {
                        self.err(
                            arm.body.pos,
                            format!(
                                "match arms have incompatible types: `{}` and `{}`",
                                display_ty(p),
                                display_ty(&arm_ty)
                            ),
                        );
                    }
                }
            }
            result = join(&result, &arm_ty);
            prev = Some(arm_ty);
            first = false;
        }
        self.check_exhaustive(&scrut, arms, pos);
        result
    }

    /// Section 7: `match` must be exhaustive. Enums need every variant (or a
    /// wildcard); `bool` needs both values; a union needs every member type;
    /// open scrutinees (integers, strings, tuples) need a `_` arm.
    pub(super) fn check_exhaustive(&mut self, scrut: &Ty, arms: &[MatchArm], pos: Pos) {
        if scrut.is_unknown() || matches!(scrut, Ty::Param(_)) {
            return;
        }
        // A guardless catch-all settles exhaustiveness immediately.
        let has_catch_all = arms.iter().any(|a| {
            a.guard.is_none() && matches!(a.pattern, Pattern::Wildcard | Pattern::Binding(_))
        });
        if has_catch_all {
            return;
        }
        match scrut {
            Ty::Enum(name, _) => {
                let variants = self.enum_variant_names(name);
                let Some(all) = variants else { return };
                let mut covered: HashSet<String> = HashSet::new();
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    collect_covered_variants(&a.pattern, &mut covered);
                }
                let missing: Vec<String> = all
                    .iter()
                    .filter(|v| !covered.contains(*v))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    self.err(
                        pos,
                        format!(
                            "non-exhaustive match: missing variant(s) {}",
                            missing.join(", ")
                        ),
                    );
                }
            }
            Ty::Bool => {
                let mut t = false;
                let mut f = false;
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    if let Pattern::Bool(b) = a.pattern {
                        if b { t = true } else { f = true }
                    }
                }
                if !(t && f) {
                    self.err(
                        pos,
                        "non-exhaustive match: `bool` needs both `true` and `false`",
                    );
                }
            }
            Ty::Union(members) => {
                let mut covered: HashSet<String> = HashSet::new();
                for a in arms {
                    if a.guard.is_some() {
                        continue;
                    }
                    if let Pattern::Typed { ty, .. } = &a.pattern {
                        covered.insert(display_ty(&self.resolve(ty)));
                    }
                    if matches!(a.pattern, Pattern::Nil) {
                        covered.insert("nil".into());
                    }
                }
                let missing: Vec<String> = members
                    .iter()
                    .map(display_ty)
                    .filter(|m| !covered.contains(m))
                    .collect();
                if !missing.is_empty() {
                    self.err(
                        pos,
                        format!(
                            "non-exhaustive match: union member(s) {} not narrowed",
                            missing.join(", ")
                        ),
                    );
                }
            }
            _ => {
                self.err(
                    pos,
                    "non-exhaustive match: add a `_` arm to cover the remaining cases",
                );
            }
        }
    }

    pub(super) fn enum_variant_names(&self, name: &str) -> Option<Vec<String>> {
        match name {
            "Option" => Some(vec!["Some".into(), "None".into()]),
            "Result" => Some(vec!["Ok".into(), "Err".into()]),
            _ => self
                .enums
                .get(name)
                .map(|e| e.variants.iter().map(|v| v.name.clone()).collect()),
        }
    }

    pub(super) fn infer_closure(
        &mut self,
        params: &[Param],
        body: &Expr,
        expected: Option<&Ty>,
    ) -> Ty {
        let expected_params: Vec<Ty> = match expected {
            Some(Ty::Fn(ps, _)) => ps.clone(),
            _ => Vec::new(),
        };
        self.push_scope();
        let mut ptys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = if let Some(t) = &p.ty {
                self.resolve(t)
            } else if let Some(t) = expected_params.get(i) {
                // Closure parameters receive the element, not a reference to it.
                strip_ref(t)
            } else {
                Ty::Unknown
            };
            self.declare(&p.name, ty.clone());
            ptys.push(ty);
        }
        let ret = self.infer(body);
        self.pop_scope();
        Ty::Fn(ptys, Box::new(ret))
    }

    pub(super) fn infer_try(&mut self, inner: &Expr, pos: Pos) -> Ty {
        let t = self.infer(inner);
        // `?` may appear only inside a function returning Result or Option.
        match self.ret_stack.last() {
            Some(Ty::Enum(n, _)) if n == "Result" || n == "Option" => {}
            Some(Ty::Unknown) | None => {}
            Some(other) => {
                let other = other.clone();
                self.err(
                    pos,
                    format!(
                        "`?` can only be used in a function returning `Result` or `Option`, not `{}`",
                        display_ty(&other)
                    ),
                );
            }
        }
        match t {
            Ty::Enum(n, args) if n == "Result" || n == "Option" => {
                args.into_iter().next().unwrap_or(Ty::Unknown)
            }
            other => other,
        }
    }

    pub(super) fn check_conformance(&mut self, b: &ImplBlock, iface: &str) {
        // The interface must exist and its super-interfaces must also be
        // implemented for the type (nominal, Section 9).
        if let Some(info) = self.interfaces.get(iface).map(|i| InterfaceInfo {
            supers: i.supers.clone(),
        }) {
            for s in &info.supers {
                if !self.satisfies(&b.ty, s) {
                    self.err(
                        b.pos,
                        format!(
                            "`{}` implements `{}` but not its super-interface `{}`",
                            b.ty, iface, s
                        ),
                    );
                }
            }
        }
    }

    pub(super) fn expect_bool(&mut self, t: &Ty, pos: Pos, ctx: &str) {
        let ok = matches!(t, Ty::Bool | Ty::Unknown) || matches!(t, Ty::Param(_));
        if !ok {
            self.err(
                pos,
                format!("{} must be `bool`, found `{}`", ctx, display_ty(t)),
            );
        }
    }
}

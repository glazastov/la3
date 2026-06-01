//! Type checker: call/method/field/index inference, bound checking and
//! conformance, and struct-literal inference. Split out of `typeck.rs`.

use std::collections::{HashMap, HashSet};

use super::*;

impl TypeChecker {
    pub(super) fn infer_call(&mut self, callee: &Expr, args: &[Expr], pos: Pos) -> Ty {
        self.check_borrow_conflicts(args, pos);
        // Enum constructors carry their payload type.
        if let ExprKind::Ident(name) = &callee.kind {
            match name.as_str() {
                "Some" => {
                    let a = args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown);
                    for a in args.iter().skip(1) {
                        self.infer(a);
                    }
                    return Ty::option(a);
                }
                "Ok" => {
                    let a = args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown);
                    for a in args.iter().skip(1) {
                        self.infer(a);
                    }
                    return Ty::result(a);
                }
                "Err" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::result(Ty::Unknown);
                }
                "str" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Str;
                }
                "len" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Int(IntKind::Usize);
                }
                "idiv" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Int(IntKind::I64);
                }
                "to_hex" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Str;
                }
                "from_hex" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::result(Ty::List(Box::new(Ty::Int(IntKind::U8))));
                }
                // Heap allocation (Section 11).
                "alloc" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Ptr(Box::new(Ty::Int(IntKind::U8)));
                }
                "dealloc" => {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Unit;
                }
                _ => {}
            }
            // A user function with a recorded signature.
            if let Some(sig) = self.fns.get(name).cloned() {
                self.check_call_args(&sig, args);
                self.check_bounds(&sig, args, pos);
                return self.instantiate_ret(&sig, args);
            }
        }

        // Associated calls like `Point.new(...)` / `TlsRecord.decode(...)`.
        let cty = self.infer(callee);
        for a in args {
            self.infer(a);
        }
        match cty {
            Ty::Fn(_, ret) => *ret,
            other if other.is_unknown() => Ty::Unknown,
            _ => Ty::Unknown,
        }
    }

    pub(super) fn check_call_args(&mut self, sig: &FnSig, args: &[Expr]) {
        for (i, a) in args.iter().enumerate() {
            let expected = sig.params.get(i).cloned();
            if let ExprKind::Closure { params, body, .. } = &a.kind {
                self.infer_closure(params, body, expected.as_ref());
            } else {
                self.infer(a);
                if let Some(p) = &expected {
                    self.pin_literals(a, p);
                }
            }
        }
    }

    /// Section 9: a generic bound `T: Iface` is satisfied only when an explicit
    /// `impl Iface for ConcreteType` exists (primitives satisfy the builtin
    /// interfaces automatically).
    pub(super) fn check_bounds(&mut self, sig: &FnSig, args: &[Expr], pos: Pos) {
        if sig.generics.iter().all(|(_, b)| b.is_empty()) {
            return;
        }
        let mut bindings: HashMap<String, Ty> = HashMap::new();
        for (i, p) in sig.params.iter().enumerate() {
            if let (Ty::Param(name), Some(arg)) = (p, args.get(i)) {
                let at = self.infer(arg);
                bindings.entry(name.clone()).or_insert(at);
            }
        }
        for (name, bounds) in &sig.generics {
            let Some(concrete) = bindings.get(name) else {
                continue;
            };
            let tyname = match concrete {
                Ty::Struct(n, _) | Ty::Enum(n, _) => n.clone(),
                _ => continue, // primitives, unknown, params: assume satisfied
            };
            for b in bounds {
                if builtin_interface(b) {
                    continue;
                }
                if !self.satisfies(&tyname, b) {
                    self.err(
                        pos,
                        format!("`{}` does not implement interface `{}`", tyname, b),
                    );
                }
            }
        }
    }

    pub(super) fn satisfies(&self, tyname: &str, iface: &str) -> bool {
        if self
            .impls
            .contains(&(iface.to_string(), tyname.to_string()))
        {
            return true;
        }
        // A combined marker interface (`interface Codec: Encode + Decode {}`) is
        // satisfied when all of its component interfaces are. A plain interface
        // with no super-interfaces requires its own explicit `impl`.
        if let Some(info) = self.interfaces.get(iface) {
            if !info.supers.is_empty() {
                return info.supers.iter().all(|s| self.satisfies(tyname, s));
            }
        }
        false
    }

    pub(super) fn instantiate_ret(&mut self, sig: &FnSig, args: &[Expr]) -> Ty {
        // Infer generic bindings from argument types, then substitute.
        let mut bindings: HashMap<String, Ty> = HashMap::new();
        for (i, p) in sig.params.iter().enumerate() {
            if let Some(arg) = args.get(i) {
                let at = self.infer(arg);
                collect_param_bindings(p, &at, &mut bindings);
            }
        }
        subst(&sig.ret, &bindings)
    }

    pub(super) fn infer_method(
        &mut self,
        recv: &Expr,
        optional: bool,
        method: &str,
        _type_args: &[TypeExpr],
        args: &[Expr],
        pos: Pos,
    ) -> Ty {
        self.check_borrow_conflicts(args, pos);
        // `module.fn(...)` (io, fs, os, math, json, bytes, crypto, net, ...).
        if let ExprKind::Ident(modname) = &recv.kind {
            if self.lookup(modname).is_none() && is_module(modname) {
                for a in args {
                    self.infer(a);
                }
                return module_fn_ret(modname, method);
            }
        }

        let rty = self.infer(recv);
        let base = if optional {
            rty.strip_nil()
        } else {
            rty.clone()
        };

        // A user method on a struct/enum.
        if let Ty::Struct(name, _) | Ty::Enum(name, _) = &base {
            if let Some(m) = self.methods.get(&(name.clone(), method.to_string())) {
                let ret = m.sig.ret.clone();
                for a in args {
                    self.infer(a);
                }
                return wrap_optional(ret, optional);
            }
        }

        // Builtin methods on str / List / Map / Set / Option / Result.
        let (param_tys, ret) = match builtin_method_sig(&base, method) {
            Some(sig) => sig,
            None => {
                // The receiver's type is known and we model its full method set,
                // yet the method does not resolve: a real error. For types whose
                // surface we don't fully model (Unknown, generics, pointers,
                // references) stay lenient and produce Unknown.
                if resolves_methods(&base) {
                    self.err(
                        pos,
                        format!("no method `{}` on type `{}`", method, display_ty(&base)),
                    );
                }
                (vec![], Ty::Unknown)
            }
        };
        for (i, a) in args.iter().enumerate() {
            let expected = param_tys.get(i).cloned();
            if let ExprKind::Closure { params, body, .. } = &a.kind {
                self.infer_closure(params, body, expected.as_ref());
            } else {
                self.infer(a);
            }
        }
        wrap_optional(ret, optional)
    }

    pub(super) fn infer_field(&mut self, recv: &Expr, optional: bool, name: &str, pos: Pos) -> Ty {
        // Module constants: `math.pi`, `math.e`, `math.inf`.
        if let ExprKind::Ident(modname) = &recv.kind {
            if self.lookup(modname).is_none() {
                if modname == "math" && matches!(name, "pi" | "e" | "inf") {
                    return Ty::Float(FloatKind::F64);
                }
                // `Enum.Variant` / `Type.assoc` accessed through an identifier.
                if let Some(t) = self.type_member(modname, name) {
                    return t;
                }
                if is_module(modname) {
                    return Ty::Unknown;
                }
            }
        }
        let rty = self.infer(recv);
        let base = if optional { rty.strip_nil() } else { rty };
        match self.field_ty(&base, name) {
            Some(t) => wrap_optional(t, optional),
            None => {
                // Known struct/tuple but no such field/index → a real error. Other
                // bases (collections, Unknown, generics) stay lenient.
                if let Ty::Struct(sname, _) = &base {
                    self.err(pos, format!("no field `{}` on struct `{}`", name, sname));
                } else if let Ty::Tuple(ts) = &base {
                    self.err(
                        pos,
                        format!("no field `{}` on tuple of {} element(s)", name, ts.len()),
                    );
                }
                wrap_optional(Ty::Unknown, optional)
            }
        }
    }

    /// `Type.member` where `Type` is a known struct or enum name.
    pub(super) fn type_member(&self, tyname: &str, member: &str) -> Option<Ty> {
        if let Some(info) = self.enums.get(tyname) {
            if let Some(v) = info.variants.iter().find(|v| v.name == member) {
                return Some(match &v.kind {
                    VariantKind::Unit => Ty::Enum(tyname.to_string(), vec![]),
                    VariantKind::Tuple(tys) => Ty::Fn(
                        vec![Ty::Unknown; tys.len()],
                        Box::new(Ty::Enum(tyname.to_string(), vec![])),
                    ),
                    VariantKind::Struct(_) => Ty::Enum(tyname.to_string(), vec![]),
                });
            }
        }
        if let Some(m) = self.methods.get(&(tyname.to_string(), member.to_string())) {
            return Some(Ty::Fn(m.sig.params.clone(), Box::new(m.sig.ret.clone())));
        }
        None
    }

    pub(super) fn field_ty(&self, base: &Ty, name: &str) -> Option<Ty> {
        match base {
            Ty::Struct(sname, args) => {
                let info = self.structs.get(sname)?;
                let (_, fty) = info.fields.iter().find(|(fname, _)| fname == name)?;
                let resolved = self.resolve_in(fty, &info.generics.iter().cloned().collect());
                let bindings: HashMap<String, Ty> = info
                    .generics
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                Some(subst(&resolved, &bindings))
            }
            Ty::Tuple(ts) => name.parse::<usize>().ok().and_then(|i| ts.get(i).cloned()),
            Ty::Ref(inner) => self.field_ty(inner, name),
            _ => None,
        }
    }

    pub(super) fn infer_index(&mut self, recv: &Expr, index: &Expr) -> Ty {
        let r = self.infer(recv);
        let is_range = matches!(index.kind, ExprKind::Range { .. });
        self.infer(index);
        match r {
            Ty::List(e) | Ty::Array(e, _) | Ty::Slice(e) => {
                if is_range {
                    Ty::List(e)
                } else {
                    *e
                }
            }
            Ty::Map(_, v) => *v,
            Ty::Str => {
                if is_range {
                    Ty::Str
                } else {
                    Ty::Char
                }
            }
            _ => Ty::Unknown,
        }
    }

    pub(super) fn infer_struct_lit(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        spread: &Option<Box<Expr>>,
        pos: Pos,
    ) -> Ty {
        let known: Option<(Vec<String>, Vec<(String, TypeExpr)>)> = self
            .structs
            .get(name)
            .map(|s| (s.generics.clone(), s.fields.clone()));
        if let Some((generics, decl_fields)) = known {
            let gset: HashSet<String> = generics.iter().cloned().collect();
            for (fname, fexpr) in fields {
                let got = self.infer(fexpr);
                match decl_fields.iter().find(|(dn, _)| dn == fname) {
                    Some((_, fty)) => {
                        let want = self.resolve_in(fty, &gset);
                        self.expect(&got, &want, fexpr.pos, &format!("field `{}`", fname));
                    }
                    None => self.err(
                        fexpr.pos,
                        format!("struct `{}` has no field `{}`", name, fname),
                    ),
                }
            }
            if let Some(s) = spread {
                self.infer(s);
            } else {
                // Every field must be supplied when there is no `..spread`.
                for (dn, _) in &decl_fields {
                    if !fields.iter().any(|(fname, _)| fname == dn) {
                        self.err(pos, format!("missing field `{}` in `{}` literal", dn, name));
                    }
                }
            }
            Ty::Struct(name.to_string(), vec![Ty::Unknown; generics.len()])
        } else {
            for (_, fexpr) in fields {
                self.infer(fexpr);
            }
            if let Some(s) = spread {
                self.infer(s);
            }
            Ty::Unknown
        }
    }
}

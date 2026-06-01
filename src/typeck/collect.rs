//! Type checker: item collection (structs, enums, impls, fn signatures) and
//! surface-syntax `TypeExpr` -> semantic `Ty` resolution. Split out of `typeck.rs`.

use std::collections::{HashMap, HashSet};

use super::*;

impl TypeChecker {
    pub(super) fn new(prog: &Program) -> Self {
        let mut tc = TypeChecker {
            structs: HashMap::new(),
            enums: HashMap::new(),
            fns: HashMap::new(),
            consts: HashMap::new(),
            aliases: HashMap::new(),
            interfaces: HashMap::new(),
            methods: HashMap::new(),
            impls: HashSet::new(),
            scopes: Vec::new(),
            type_params: HashSet::new(),
            ret_stack: Vec::new(),
            loop_breaks: Vec::new(),
            unsafe_depth: 0,
            types: HashMap::new(),
            type_order: Vec::new(),
            errors: Vec::new(),
        };
        tc.collect(prog);
        tc
    }

    /// First pass: record every top-level declaration so forward references and
    /// mutual recursion resolve.
    pub(super) fn collect(&mut self, prog: &Program) {
        // Type-shaped declarations first, so signatures can resolve them.
        for item in &prog.items {
            match item {
                Item::Struct(s) => {
                    self.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            generics: s.generics.clone(),
                            fields: s.fields.clone(),
                        },
                    );
                }
                Item::Enum(e) => {
                    self.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            generics: e.generics.clone(),
                            variants: e.variants.clone(),
                        },
                    );
                }
                Item::TypeAlias { name, ty } => {
                    self.aliases.insert(name.clone(), ty.clone());
                }
                Item::Interface(i) => {
                    self.interfaces.insert(
                        i.name.clone(),
                        InterfaceInfo {
                            supers: i.supers.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        // Now functions, consts, and impl blocks, whose signatures reference types.
        for item in &prog.items {
            match item {
                Item::Fn(f) => {
                    let sig = self.fn_sig(f, &HashSet::new());
                    self.fns.insert(f.name.clone(), sig);
                }
                Item::Const(c) => {
                    let ty =
                        c.ty.as_ref()
                            .map(|t| self.resolve(t))
                            .unwrap_or(Ty::Unknown);
                    self.consts.insert(c.name.clone(), ty);
                }
                Item::Impl(b) => {
                    let owner_generics: HashSet<String> = self
                        .structs
                        .get(&b.ty)
                        .map(|s| s.generics.clone())
                        .or_else(|| self.enums.get(&b.ty).map(|e| e.generics.clone()))
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    if let Some(iface) = &b.interface {
                        self.impls.insert((iface.clone(), b.ty.clone()));
                    }
                    for m in &b.methods {
                        let sig = self.fn_sig(m, &owner_generics);
                        self.methods
                            .insert((b.ty.clone(), m.name.clone()), MethodSig { sig });
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn fn_sig(&self, f: &FnDecl, extra_generics: &HashSet<String>) -> FnSig {
        let mut generics_set: HashSet<String> = extra_generics.clone();
        for (g, _) in &f.generics {
            generics_set.insert(g.clone());
        }
        let params = f
            .params
            .iter()
            .filter(|p| !p.is_self)
            .map(|p| {
                p.ty.as_ref()
                    .map(|t| self.resolve_in(t, &generics_set))
                    .unwrap_or(Ty::Unknown)
            })
            .collect();
        let mut ret = f
            .ret
            .as_ref()
            .map(|t| self.resolve_in(t, &generics_set))
            .unwrap_or(Ty::Unit);
        if f.is_async {
            ret = Ty::Future(Box::new(ret));
        }
        let generics = f.generics.clone();
        FnSig {
            generics,
            params,
            ret,
        }
    }

    // ---- type resolution -------------------------------------------------

    pub(super) fn resolve(&self, t: &TypeExpr) -> Ty {
        let params = self.type_params.clone();
        self.resolve_in(t, &params)
    }

    pub(super) fn resolve_in(&self, t: &TypeExpr, generics: &HashSet<String>) -> Ty {
        match t {
            TypeExpr::Named { name, args } => {
                let rargs: Vec<Ty> = args.iter().map(|a| self.resolve_in(a, generics)).collect();
                if generics.contains(name) && rargs.is_empty() {
                    return Ty::Param(name.clone());
                }
                match name.as_str() {
                    "bool" => Ty::Bool,
                    "char" => Ty::Char,
                    "str" => Ty::Str,
                    "nil" => Ty::Nil,
                    "f32" => Ty::Float(FloatKind::F32),
                    "f64" => Ty::Float(FloatKind::F64),
                    "List" | "Vec" => Ty::List(Box::new(arg0(&rargs))),
                    "Set" => Ty::Set(Box::new(arg0(&rargs))),
                    "Map" => Ty::Map(Box::new(arg0(&rargs)), Box::new(argn(&rargs, 1))),
                    "Option" => Ty::option(arg0(&rargs)),
                    "Result" => Ty::result(arg0(&rargs)),
                    "any" => Ty::Unknown,
                    _ => {
                        if let Some(k) = int_kind(name) {
                            Ty::Int(k)
                        } else if let Some(alias) = self.aliases.get(name) {
                            self.resolve_in(&alias.clone(), generics)
                        } else if self.structs.contains_key(name) {
                            Ty::Struct(name.clone(), rargs)
                        } else if self.enums.contains_key(name) {
                            Ty::Enum(name.clone(), rargs)
                        } else {
                            // Interfaces used as a type, or anything unknown.
                            Ty::Unknown
                        }
                    }
                }
            }
            TypeExpr::Ref { inner, .. } => Ty::Ref(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Ptr { inner, .. } => Ty::Ptr(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Array { inner, size } => Ty::Array(
                Box::new(self.resolve_in(inner, generics)),
                size.map(|s| s as usize),
            ),
            TypeExpr::Slice(inner) => Ty::Slice(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Tuple(ts) => {
                Ty::Tuple(ts.iter().map(|t| self.resolve_in(t, generics)).collect())
            }
            TypeExpr::Union(ts) => {
                let members: Vec<Ty> = ts.iter().map(|t| self.resolve_in(t, generics)).collect();
                normalize_union(members)
            }
            TypeExpr::Fn { params, ret } => Ty::Fn(
                params
                    .iter()
                    .map(|t| self.resolve_in(t, generics))
                    .collect(),
                Box::new(self.resolve_in(ret, generics)),
            ),
            TypeExpr::Async(inner) => Ty::Future(Box::new(self.resolve_in(inner, generics))),
            TypeExpr::Unit => Ty::Unit,
            TypeExpr::Never => Ty::Never,
        }
    }
}

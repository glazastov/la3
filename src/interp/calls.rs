//! Interpreter: calls (free functions, closures, methods), field access,
//! method dispatch, and indexing. Split out of `interp.rs`.

use std::rc::Rc;

use super::*;

impl Interp {
    // ---- calls ----

    pub(super) fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        // Enum variant or builtin free function by name.
        if let ExprKind::Ident(name) = &callee.kind {
            if lookup(env, name).is_none() {
                if let Some(v) = self.try_builtin_fn(name, args, env, pos)? {
                    return Ok(v);
                }
                if let Some(owner) = self.variant_owner.get(name).cloned() {
                    let mut data = Vec::new();
                    for a in args {
                        data.push(self.eval(a, env)?);
                    }
                    if name == "None" {
                        return Ok(Value::Nil);
                    }
                    return Ok(Value::Enum {
                        ty: Rc::new(owner),
                        variant: Rc::new(name.clone()),
                        data: Rc::new(data),
                    });
                }
            }
        }
        if let ExprKind::Path(segs) = &callee.kind {
            // `Enum::Variant(args)`
            if segs.len() == 2
                && (self.enums.contains_key(&segs[0]) || segs[0] == "Option" || segs[0] == "Result")
            {
                let mut data = Vec::new();
                for a in args {
                    data.push(self.eval(a, env)?);
                }
                return Ok(Value::Enum {
                    ty: Rc::new(segs[0].clone()),
                    variant: Rc::new(segs[1].clone()),
                    data: Rc::new(data),
                });
            }
        }
        let f = self.eval(callee, env)?;
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval(a, env)?);
        }
        self.call_value(f, argv, pos)
    }

    pub(super) fn call_value(&mut self, f: Value, args: Vec<Value>, pos: Pos) -> R<Value> {
        match f {
            Value::Function(decl) => self.call_fn(&decl, None, args, pos),
            Value::Closure(c) => {
                let scope = new_scope(Some(c.env.clone()));
                bind_params(&c.params, None, args, &scope);
                match self.eval(&c.body, &scope) {
                    Err(Signal::Return(v)) => Ok(v),
                    other => other,
                }
            }
            _ => rt(pos, format!("{} is not callable", f.type_name())),
        }
    }

    pub(super) fn call_fn(
        &mut self,
        decl: &FnDecl,
        self_val: Option<Value>,
        args: Vec<Value>,
        pos: Pos,
    ) -> R<Value> {
        let scope = new_scope(Some(self.globals.clone()));
        // variadic handling
        let fixed: Vec<&Param> = decl.params.iter().filter(|p| !p.is_self).collect();
        if decl.variadic.is_some() {
            let n = fixed.len();
            let (head, tail) = if args.len() >= n {
                args.split_at(n)
            } else {
                (&args[..], &[][..])
            };
            // bind self
            if let Some(sv) = self_val {
                define(&scope, "self", sv, true);
            }
            for (p, a) in fixed.iter().zip(head.iter()) {
                define(&scope, &p.name, a.clone(), true);
            }
            let v = decl.variadic.as_ref().unwrap();
            define(&scope, &v.name, list_val(tail.to_vec()), false);
        } else {
            bind_params(&decl.params, self_val, args, &scope);
        }
        let _ = pos;
        match self.eval_block(&decl.body, &scope) {
            Err(Signal::Return(v)) => Ok(v),
            other => other,
        }
    }

    // ---- field / method / index ----

    pub(super) fn eval_field(
        &mut self,
        recv: &Expr,
        optional: bool,
        name: &str,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        // `EnumName.Variant` unit variant.
        if let ExprKind::Ident(tyname) = &recv.kind {
            if lookup(env, tyname).is_none()
                && (self.enums.contains_key(tyname) || tyname == "Option" || tyname == "Result")
            {
                return Ok(Value::Enum {
                    ty: Rc::new(tyname.clone()),
                    variant: Rc::new(name.to_string()),
                    data: Rc::new(vec![]),
                });
            }
            // module constants e.g. math.pi
            if let Some(v) = module_const(tyname, name) {
                return Ok(v);
            }
        }
        let mut obj = self.eval(recv, env)?;
        // A reference is transparent for field access (`r.field` reaches through
        // `&T`/`&mut T` to the pointee), matching the safe-reference model.
        while let Value::Ref(cell, _) = obj {
            obj = cell.borrow().clone();
        }
        if optional && matches!(obj, Value::Nil) {
            return Ok(Value::Nil);
        }
        match &obj {
            Value::Struct { fields, .. } => {
                Ok(fields.borrow().get(name).cloned().unwrap_or(Value::Nil))
            }
            Value::Tuple(items) => {
                if let Ok(i) = name.parse::<usize>() {
                    Ok(items.get(i).cloned().unwrap_or(Value::Nil))
                } else {
                    rt(pos, "tuple fields are numeric")
                }
            }
            _ => rt(
                pos,
                format!("type {} has no field '{}'", obj.type_name(), name),
            ),
        }
    }

    pub(super) fn eval_method(
        &mut self,
        recv: &Expr,
        optional: bool,
        method: &str,
        args: &[Expr],
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        // Static dispatch: `Type.assoc(...)`, `Enum.Variant(...)`, `module.fn(...)`.
        if let ExprKind::Ident(tyname) = &recv.kind {
            if lookup(env, tyname).is_none() {
                // module function
                if is_module(tyname) {
                    let mut argv = Vec::new();
                    for a in args {
                        argv.push(self.eval(a, env)?);
                    }
                    return self.call_module(tyname, method, argv, pos);
                }
                // enum variant constructor
                if self.enums.contains_key(tyname) || tyname == "Option" || tyname == "Result" {
                    // could also be an associated fn; prefer variant if it exists
                    let is_variant = self
                        .enums
                        .get(tyname)
                        .map(|e| e.variants.iter().any(|v| v.name == method))
                        .unwrap_or(
                            method == "Some"
                                || method == "None"
                                || method == "Ok"
                                || method == "Err",
                        );
                    if is_variant {
                        let mut data = Vec::new();
                        for a in args {
                            data.push(self.eval(a, env)?);
                        }
                        if method == "None" {
                            return Ok(Value::Nil);
                        }
                        return Ok(Value::Enum {
                            ty: Rc::new(tyname.clone()),
                            variant: Rc::new(method.to_string()),
                            data: Rc::new(data),
                        });
                    }
                }
                // associated function (no self) on a struct/enum
                if let Some(decl) = self.find_method(tyname, method) {
                    let mut argv = Vec::new();
                    for a in args {
                        argv.push(self.eval(a, env)?);
                    }
                    return self.call_fn(&decl, None, argv, pos);
                }
            }
        }

        let obj = self.eval(recv, env)?;
        if optional && matches!(obj, Value::Nil) {
            return Ok(Value::Nil);
        }
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval(a, env)?);
        }

        // User-defined instance methods take priority for struct/enum receivers.
        let tyname = match &obj {
            Value::Struct { name, .. } => Some(name.to_string()),
            Value::Enum { ty, .. } => Some(ty.to_string()),
            _ => None,
        };
        if let Some(tn) = &tyname {
            if let Some(decl) = self.find_method(tn, method) {
                return self.call_fn(&decl, Some(obj.clone()), argv, pos);
            }
        }

        self.builtin_method(obj, method, argv, env, pos)
    }

    pub(super) fn find_method(&self, ty: &str, name: &str) -> Option<Rc<FnDecl>> {
        self.methods.get(ty).and_then(|m| m.get(name)).cloned()
    }

    pub(super) fn eval_index(
        &mut self,
        recv: &Expr,
        index: &Expr,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        let c = self.eval(recv, env)?;
        let i = self.eval(index, env)?;
        match &c {
            Value::List(l) => {
                if let Value::Range {
                    start,
                    end,
                    inclusive,
                } = i
                {
                    let b = l.borrow();
                    let hi =
                        (if inclusive { end + 1 } else { end }).clamp(0, b.len() as i64) as usize;
                    let lo = start.clamp(0, b.len() as i64) as usize;
                    Ok(list_val(b[lo..hi.max(lo)].to_vec()))
                } else {
                    let idx = self.as_int(&i, pos)?;
                    let b = l.borrow();
                    b.get(idx as usize).cloned().ok_or_else(|| {
                        Signal::Error(Diagnostic::new(
                            Phase::Runtime,
                            pos,
                            "list index out of bounds",
                        ))
                    })
                }
            }
            Value::Map(m) => {
                let b = m.borrow();
                b.iter()
                    .find(|(k, _)| value_eq(k, &i))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| {
                        Signal::Error(Diagnostic::new(
                            Phase::Runtime,
                            pos,
                            "key not present in map",
                        ))
                    })
            }
            Value::Str(s) => {
                let idx = self.as_int(&i, pos)?;
                s.chars().nth(idx as usize).map(Value::Char).ok_or_else(|| {
                    Signal::Error(Diagnostic::new(
                        Phase::Runtime,
                        pos,
                        "string index out of bounds",
                    ))
                })
            }
            _ => rt(pos, format!("cannot index {}", c.type_name())),
        }
    }
}

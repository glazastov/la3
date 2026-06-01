//! Interpreter: expression evaluation — the `eval` dispatcher, identifiers,
//! paths, unary/binary operators, references and raw pointers, assignment,
//! and struct literals. Split out of `interp.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::*;

impl Interp {
    // ---- expressions ----

    pub(super) fn eval(&mut self, expr: &Expr, env: &Env) -> R<Value> {
        let pos = expr.pos;
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Str(s) => Ok(str_val(s.clone())),
            ExprKind::Char(c) => Ok(Value::Char(*c)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Nil => Ok(Value::Nil),
            ExprKind::FStr(parts) => self.eval_fstring(parts, env, pos),
            ExprKind::SelfExpr => self.read_ident("self", env, pos),
            ExprKind::Ident(name) => self.read_ident(name, env, pos),
            ExprKind::Path(segs) => self.eval_path(segs, env, pos),
            ExprKind::Unary { op, expr } => self.eval_unary(*op, expr, env, pos),
            ExprKind::Binary { op, lhs, rhs } => self.eval_binary(*op, lhs, rhs, env, pos),
            ExprKind::Coalesce { lhs, rhs } => {
                let l = self.eval(lhs, env)?;
                if matches!(l, Value::Nil) {
                    self.eval(rhs, env)
                } else {
                    Ok(l)
                }
            }
            ExprKind::Assign { target, op, value } => {
                self.eval_assign(target, *op, value, env, pos)
            }
            ExprKind::Cast { expr, ty } => {
                let v = self.eval(expr, env)?;
                self.cast(v, ty, pos)
            }
            ExprKind::Tuple(parts) => {
                let mut vs = Vec::with_capacity(parts.len());
                for p in parts {
                    vs.push(self.eval(p, env)?);
                }
                if vs.is_empty() {
                    Ok(Value::Unit)
                } else {
                    Ok(Value::Tuple(Rc::new(vs)))
                }
            }
            ExprKind::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for it in items {
                    vs.push(self.eval(it, env)?);
                }
                Ok(list_val(vs))
            }
            ExprKind::ListRepeat { value, count } => {
                let v = self.eval(value, env)?;
                let c = self.eval(count, env)?;
                let n = self.as_int(&c, pos)?;
                Ok(list_val(vec![v; n.max(0) as usize]))
            }
            ExprKind::Map(entries) => {
                let mut m = Vec::new();
                for (k, val) in entries {
                    let kv = self.eval(k, env)?;
                    let vv = self.eval(val, env)?;
                    m.push((kv, vv));
                }
                Ok(Value::Map(Rc::new(RefCell::new(m))))
            }
            ExprKind::Set(items) => {
                let mut vs = Vec::new();
                for it in items {
                    let v = self.eval(it, env)?;
                    if !vs.iter().any(|e| value_eq(e, &v)) {
                        vs.push(v);
                    }
                }
                Ok(Value::Set(Rc::new(RefCell::new(vs))))
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let s = self.eval(start, env)?;
                let e = self.eval(end, env)?;
                Ok(Value::Range {
                    start: self.as_int(&s, pos)?,
                    end: self.as_int(&e, pos)?,
                    inclusive: *inclusive,
                })
            }
            ExprKind::StructLit {
                name,
                fields,
                spread,
            } => self.eval_struct_lit(name, fields, spread, env, pos),
            ExprKind::Block(b) => self.eval_block(b, env),
            ExprKind::If { cond, then, els } => {
                let c = self.eval(cond, env)?;
                if self.as_bool(&c, pos)? {
                    self.eval_block(then, env)
                } else if let Some(e) = els {
                    self.eval(e, env)
                } else {
                    Ok(Value::Unit)
                }
            }
            ExprKind::Match { scrutinee, arms } => self.eval_match(scrutinee, arms, env, pos),
            ExprKind::Loop { body } => self.eval_loop(body, env),
            ExprKind::While { cond, body } => self.eval_while(cond, body, env, pos),
            ExprKind::WhileLet {
                pattern,
                expr,
                body,
            } => self.eval_while_let(pattern, expr, body, env),
            ExprKind::For {
                pattern,
                iter,
                body,
            } => self.eval_for(pattern, iter, body, env, pos),
            ExprKind::Closure { params, body, .. } => Ok(Value::Closure(Rc::new(ClosureData {
                params: params.clone(),
                body: (**body).clone(),
                env: env.clone(),
            }))),
            ExprKind::Call { callee, args } => self.eval_call(callee, args, env, pos),
            ExprKind::MethodCall {
                recv,
                optional,
                method,
                args,
                ..
            } => self.eval_method(recv, *optional, method, args, env, pos),
            ExprKind::Field {
                recv,
                optional,
                name,
            } => self.eval_field(recv, *optional, name, env, pos),
            ExprKind::Index { recv, index } => self.eval_index(recv, index, env, pos),
            ExprKind::Try(inner) => {
                let v = self.eval(inner, env)?;
                match &v {
                    Value::Nil => Err(Signal::Return(Value::Nil)),
                    Value::Enum { variant, data, .. } => match variant.as_str() {
                        "Ok" | "Some" => Ok(data.get(0).cloned().unwrap_or(Value::Unit)),
                        "Err" => Err(Signal::Return(v.clone())),
                        "None" => Err(Signal::Return(Value::Nil)),
                        _ => rt(pos, "`?` expects a Result or Option"),
                    },
                    _ => rt(pos, "`?` expects a Result or Option"),
                }
            }
            ExprKind::Await(inner) => {
                let v = self.eval(inner, env)?;
                self.force(v, pos)
            }
            ExprKind::Spawn(body) => {
                // Defer the task; it runs when first forced (join/await/recv) or
                // when the program drains remaining tasks at shutdown.
                let task = Rc::new(TaskState {
                    body: body.clone(),
                    env: env.clone(),
                    result: RefCell::new(None),
                });
                self.ready.push_back(task.clone());
                Ok(Value::Future(task))
            }
            ExprKind::Unsafe(body) => self.eval_block(body, env),
            ExprKind::TryCatch {
                body,
                catches,
                finally,
            } => {
                let result = self.eval_block(body, env);
                let out = match result {
                    Err(Signal::Error(d)) => {
                        // Route a runtime error into the first catch arm.
                        if let Some(arm) = catches.first() {
                            let cenv = new_scope(Some(env.clone()));
                            if let Some(b) = &arm.binding {
                                let exc = make_exception(&d.message);
                                define(&cenv, b, exc, false);
                            }
                            self.eval_block_in(&arm.body, &cenv)
                        } else {
                            Err(Signal::Error(d))
                        }
                    }
                    other => other,
                };
                if let Some(f) = finally {
                    self.eval_block(f, env)?;
                }
                out
            }
        }
    }

    pub(super) fn read_ident(&mut self, name: &str, env: &Env, pos: Pos) -> R<Value> {
        if let Some(cell) = lookup(env, name) {
            return Ok(cell.borrow().clone());
        }
        // Bare enum variant with no data, e.g. `None`.
        if name == "None" {
            return Ok(Value::Nil);
        }
        if let Some(owner) = self.variant_owner.get(name).cloned() {
            return Ok(Value::Enum {
                ty: Rc::new(owner),
                variant: Rc::new(name.to_string()),
                data: Rc::new(vec![]),
            });
        }
        rt(pos, format!("undefined name '{}'", name))
    }

    pub(super) fn eval_path(&mut self, segs: &[String], env: &Env, pos: Pos) -> R<Value> {
        // `Enum::Variant` -> variant value; otherwise treat last as a name.
        if segs.len() == 2 {
            if self.enums.contains_key(&segs[0]) || segs[0] == "Option" || segs[0] == "Result" {
                return Ok(Value::Enum {
                    ty: Rc::new(segs[0].clone()),
                    variant: Rc::new(segs[1].clone()),
                    data: Rc::new(vec![]),
                });
            }
        }
        self.read_ident(&segs[segs.len() - 1], env, pos)
    }

    pub(super) fn eval_unary(&mut self, op: UnOp, expr: &Expr, env: &Env, pos: Pos) -> R<Value> {
        match op {
            // Safe references alias the referent's storage cell so writes are
            // visible through the original binding (Section 11).
            UnOp::Ref | UnOp::RefMut => self.eval_ref(expr, env, matches!(op, UnOp::RefMut), pos),
            // `&raw` takes a raw pointer into an array/list slot or a boxed value.
            UnOp::RawRef => self.eval_raw_ref(expr, env, pos),
            // `*` reads through a safe reference or a raw pointer.
            UnOp::Deref => {
                let v = self.eval(expr, env)?;
                self.deref(&v, pos)
            }
            UnOp::Neg => match self.eval(expr, env)? {
                Value::Int(n) => Ok(Value::Int(-n)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => rt(pos, "unary '-' expects a number"),
            },
            UnOp::Not => {
                let v = self.eval(expr, env)?;
                Ok(Value::Bool(!self.as_bool(&v, pos)?))
            }
            UnOp::BitNot => match self.eval(expr, env)? {
                Value::Int(n) => Ok(Value::Int(!n)),
                _ => rt(pos, "'~' expects an integer"),
            },
        }
    }

    /// `&x` / `&mut x`: a safe reference. When the operand names a binding, the
    /// reference shares that binding's cell so writes are visible through it;
    /// otherwise the value is boxed in a fresh cell.
    pub(super) fn eval_ref(&mut self, expr: &Expr, env: &Env, mutable: bool, pos: Pos) -> R<Value> {
        if let ExprKind::Ident(name) = &expr.kind {
            if let Some(cell) = lookup(env, name) {
                return Ok(Value::Ref(cell, mutable));
            }
        }
        let v = self.eval(expr, env)?;
        let _ = pos;
        Ok(Value::Ref(Rc::new(RefCell::new(v)), mutable))
    }

    /// `&raw expr`: a raw pointer. Into an array/list slot when the operand is an
    /// index expression (`&raw arr[i]`), otherwise into a one-element region
    /// boxing the value.
    pub(super) fn eval_raw_ref(&mut self, expr: &Expr, env: &Env, pos: Pos) -> R<Value> {
        if let ExprKind::Index { recv, index } = &expr.kind {
            let base = self.eval(recv, env)?;
            let idx = self.eval(index, env)?;
            let i = self.as_int(&idx, pos)?;
            if let Some((store, elem_size)) = self.ptr_target(&base) {
                return Ok(self.make_ptr(store, i, elem_size, true));
            }
        }
        let v = self.eval(expr, env)?;
        if let Some((store, elem_size)) = self.ptr_target(&v) {
            return Ok(self.make_ptr(store, 0, elem_size, true));
        }
        let elem_size = size_of_value(&v);
        let store = Rc::new(RefCell::new(vec![v]));
        Ok(self.make_ptr(store, 0, elem_size, true))
    }

    /// The backing store and element size for a value a raw pointer can address.
    pub(super) fn ptr_target(&self, v: &Value) -> Option<(Rc<RefCell<Vec<Value>>>, usize)> {
        match v {
            Value::List(l) | Value::Set(l) => {
                let elem = l.borrow().first().map(size_of_value).unwrap_or(1);
                Some((l.clone(), elem))
            }
            Value::Ptr(p) => Some((p.store.clone(), p.elem_size)),
            _ => None,
        }
    }

    pub(super) fn make_ptr(
        &self,
        store: Rc<RefCell<Vec<Value>>>,
        index: i64,
        elem_size: usize,
        mutable: bool,
    ) -> Value {
        Value::Ptr(Rc::new(PtrData {
            store,
            index,
            elem_size,
            mutable,
        }))
    }

    /// Read through a safe reference or a raw pointer.
    pub(super) fn deref(&self, v: &Value, pos: Pos) -> R<Value> {
        match v {
            Value::Ref(cell, _) => Ok(cell.borrow().clone()),
            Value::Ptr(p) => {
                let store = p.store.borrow();
                if p.index < 0 || p.index as usize >= store.len() {
                    return rt(pos, "raw pointer dereference out of bounds");
                }
                Ok(store[p.index as usize].clone())
            }
            other => Ok(other.clone()),
        }
    }

    /// Write `new` through a safe reference or a raw pointer (`*r = v`).
    pub(super) fn write_through(&self, target: &Value, new: Value, pos: Pos) -> R<()> {
        match target {
            Value::Ref(cell, mutable) => {
                if !*mutable {
                    return rt(pos, "cannot assign through a shared reference `&T`");
                }
                *cell.borrow_mut() = new;
                Ok(())
            }
            Value::Ptr(p) => {
                if !p.mutable {
                    return rt(pos, "cannot assign through a `*const` pointer");
                }
                let mut store = p.store.borrow_mut();
                if p.index < 0 || p.index as usize >= store.len() {
                    return rt(pos, "raw pointer write out of bounds");
                }
                store[p.index as usize] = new;
                Ok(())
            }
            _ => rt(pos, "cannot assign through a non-reference value"),
        }
    }

    pub(super) fn eval_binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        // Short-circuit logical operators (strictly bool).
        if matches!(op, BinOp::And | BinOp::Or) {
            let l = self.eval(lhs, env)?;
            let lb = self.as_bool(&l, pos)?;
            if op == BinOp::And && !lb {
                return Ok(Value::Bool(false));
            }
            if op == BinOp::Or && lb {
                return Ok(Value::Bool(true));
            }
            let r = self.eval(rhs, env)?;
            return Ok(Value::Bool(self.as_bool(&r, pos)?));
        }

        let l = self.eval(lhs, env)?;
        let r = self.eval(rhs, env)?;

        match op {
            BinOp::Eq => return Ok(Value::Bool(value_eq(&l, &r))),
            BinOp::Ne => return Ok(Value::Bool(!value_eq(&l, &r))),
            _ => {}
        }

        // Pointer arithmetic, scaled by element (Section 11): `p + n` selects the
        // element n steps along, matching `arr[i] == *(arr_ptr + i)`.
        if matches!(op, BinOp::Add | BinOp::Sub) {
            match (&l, &r) {
                (Value::Ptr(p), Value::Int(n)) | (Value::Int(n), Value::Ptr(p)) => {
                    let step = if op == BinOp::Sub { -n } else { *n };
                    return Ok(self.make_ptr(
                        p.store.clone(),
                        p.index + step,
                        p.elem_size,
                        p.mutable,
                    ));
                }
                // `q - p` is the element distance between two pointers.
                (Value::Ptr(a), Value::Ptr(b)) if op == BinOp::Sub => {
                    return Ok(Value::Int(a.index - b.index));
                }
                _ => {}
            }
        }

        // String concatenation with `+`.
        if op == BinOp::Add {
            if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
                return Ok(str_val(format!("{}{}", a, b)));
            }
        }

        // Comparisons allow int/int, float/float, str/str, char/char.
        if matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge) {
            return self.compare(op, &l, &r, pos);
        }

        // Exponentiation always yields f64.
        if op == BinOp::Pow {
            let a = self.as_f64(&l, pos)?;
            let b = self.as_f64(&r, pos)?;
            return Ok(Value::Float(a.powf(b)));
        }

        // Bitwise on integers.
        if matches!(
            op,
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
        ) {
            let a = self.as_int(&l, pos)?;
            let b = self.as_int(&r, pos)?;
            let v = match op {
                BinOp::BitAnd => a & b,
                BinOp::BitOr => a | b,
                BinOp::BitXor => a ^ b,
                BinOp::Shl => a << b,
                BinOp::Shr => a >> b,
                _ => unreachable!(),
            };
            return Ok(Value::Int(v));
        }

        // Arithmetic: int/int stays int; float/float stays float; mixing errors.
        match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    BinOp::Add => a.wrapping_add(*b),
                    BinOp::Sub => a.wrapping_sub(*b),
                    BinOp::Mul => a.wrapping_mul(*b),
                    BinOp::Div => {
                        if *b == 0 {
                            return rt(pos, "integer division by zero");
                        }
                        a / b
                    }
                    BinOp::Rem => {
                        if *b == 0 {
                            return rt(pos, "remainder by zero");
                        }
                        a % b
                    }
                    _ => return rt(pos, "unsupported integer operator"),
                };
                Ok(Value::Int(v))
            }
            (Value::Float(a), Value::Float(b)) => {
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    _ => return rt(pos, "unsupported float operator"),
                };
                Ok(Value::Float(v))
            }
            _ => rt(
                pos,
                format!(
                    "mixed-type arithmetic between {} and {} needs an explicit `as` cast",
                    l.type_name(),
                    r.type_name()
                ),
            ),
        }
    }

    pub(super) fn compare(&self, op: BinOp, l: &Value, r: &Value, pos: Pos) -> R<Value> {
        let ord = match (l, r) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.partial_cmp(b),
            (Value::Char(a), Value::Char(b)) => a.partial_cmp(b),
            _ => {
                return rt(
                    pos,
                    format!("cannot compare {} and {}", l.type_name(), r.type_name()),
                );
            }
        };
        let res = match ord {
            Some(o) => match op {
                BinOp::Lt => o.is_lt(),
                BinOp::Gt => o.is_gt(),
                BinOp::Le => o.is_le(),
                BinOp::Ge => o.is_ge(),
                _ => unreachable!(),
            },
            None => false,
        };
        Ok(Value::Bool(res))
    }

    pub(super) fn eval_assign(
        &mut self,
        target: &Expr,
        op: Option<BinOp>,
        value: &Expr,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        let rhs = self.eval(value, env)?;
        match &target.kind {
            ExprKind::Ident(name) => {
                let (cell, mutable) = lookup_var(env, name).ok_or_else(|| {
                    Signal::Error(Diagnostic::new(
                        Phase::Runtime,
                        pos,
                        format!("undefined name '{}'", name),
                    ))
                })?;
                if !mutable {
                    return rt(
                        pos,
                        format!(
                            "cannot assign to immutable binding '{}' (declare it `let mut`)",
                            name
                        ),
                    );
                }
                let new = self.apply_compound(op, cell.borrow().clone(), rhs, pos)?;
                *cell.borrow_mut() = new;
                Ok(Value::Unit)
            }
            ExprKind::Index { recv, index } => {
                let container = self.eval(recv, env)?;
                let idx = self.eval(index, env)?;
                match container {
                    Value::List(l) => {
                        let i = self.as_int(&idx, pos)? as usize;
                        let mut b = l.borrow_mut();
                        if i >= b.len() {
                            return rt(pos, "list index out of bounds");
                        }
                        let new = self.apply_compound(op, b[i].clone(), rhs, pos)?;
                        b[i] = new;
                        Ok(Value::Unit)
                    }
                    Value::Map(m) => {
                        let mut b = m.borrow_mut();
                        if let Some(slot) = b.iter_mut().find(|(k, _)| value_eq(k, &idx)) {
                            let new = self.apply_compound(op, slot.1.clone(), rhs, pos)?;
                            slot.1 = new;
                        } else {
                            let new = self.apply_compound(op, Value::Nil, rhs, pos)?;
                            b.push((idx, new));
                        }
                        Ok(Value::Unit)
                    }
                    _ => rt(pos, "cannot index-assign this value"),
                }
            }
            ExprKind::Field { recv, name, .. } => {
                let obj = self.eval(recv, env)?;
                if let Value::Struct { fields, .. } = obj {
                    let cur = fields.borrow().get(name).cloned().unwrap_or(Value::Nil);
                    let new = self.apply_compound(op, cur, rhs, pos)?;
                    fields.borrow_mut().insert(name.clone(), new);
                    Ok(Value::Unit)
                } else {
                    rt(pos, "cannot assign to a field of a non-struct value")
                }
            }
            // `*r = v` / `*p += n`: write through a reference or raw pointer.
            ExprKind::Unary {
                op: UnOp::Deref,
                expr,
            } => {
                let target = self.eval(expr, env)?;
                let cur = self.deref(&target, pos)?;
                let new = self.apply_compound(op, cur, rhs, pos)?;
                self.write_through(&target, new, pos)?;
                Ok(Value::Unit)
            }
            _ => rt(pos, "invalid assignment target"),
        }
    }

    pub(super) fn apply_compound(
        &self,
        op: Option<BinOp>,
        cur: Value,
        rhs: Value,
        pos: Pos,
    ) -> R<Value> {
        match op {
            None => Ok(rhs),
            Some(b) => match (cur, rhs) {
                (Value::Int(a), Value::Int(c)) => Ok(Value::Int(match b {
                    BinOp::Add => a + c,
                    BinOp::Sub => a - c,
                    BinOp::Mul => a * c,
                    BinOp::Div => a / c,
                    BinOp::Rem => a % c,
                    _ => return rt(pos, "bad compound operator"),
                })),
                (Value::Float(a), Value::Float(c)) => Ok(Value::Float(match b {
                    BinOp::Add => a + c,
                    BinOp::Sub => a - c,
                    BinOp::Mul => a * c,
                    BinOp::Div => a / c,
                    BinOp::Rem => a % c,
                    _ => return rt(pos, "bad compound operator"),
                })),
                (Value::Str(a), Value::Str(c)) if b == BinOp::Add => {
                    Ok(str_val(format!("{}{}", a, c)))
                }
                _ => rt(pos, "type mismatch in compound assignment"),
            },
        }
    }

    pub(super) fn eval_struct_lit(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        spread: &Option<Box<Expr>>,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        let mut map = HashMap::new();
        if let Some(sp) = spread {
            let base = self.eval(sp, env)?;
            if let Value::Struct { fields: bf, .. } = base {
                for (k, v) in bf.borrow().iter() {
                    map.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, e) in fields {
            let v = self.eval(e, env)?;
            map.insert(k.clone(), v);
        }
        let _ = pos;
        Ok(Value::Struct {
            name: Rc::new(name.to_string()),
            fields: Rc::new(RefCell::new(map)),
        })
    }
}

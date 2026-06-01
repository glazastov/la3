//! Interpreter: statement and block execution, plus irrefutable pattern
//! binding (`let`). Split out of `interp.rs`; methods are `pub(super)`.

use std::cell::RefCell;
use std::rc::Rc;

use super::*;

impl Interp {
    // ---- statements / blocks ----

    pub(super) fn eval_block(&mut self, block: &Block, parent: &Env) -> R<Value> {
        let env = new_scope(Some(parent.clone()));
        self.eval_block_in(block, &env)
    }

    pub(super) fn eval_block_in(&mut self, block: &Block, env: &Env) -> R<Value> {
        for stmt in &block.stmts {
            self.eval_stmt(stmt, env)?;
        }
        if let Some(tail) = &block.tail {
            self.eval(tail, env)
        } else {
            Ok(Value::Unit)
        }
    }

    pub(super) fn eval_stmt(&mut self, stmt: &Stmt, env: &Env) -> R<()> {
        match stmt {
            Stmt::Let {
                pattern,
                mutable,
                ty,
                value,
                ..
            } => {
                let mut v = self.eval(value, env)?;
                // An empty `{}` is a Map by default; coerce it to a Set when the
                // binding is annotated `Set<...>`.
                if let Some(TypeExpr::Named { name, .. }) = ty {
                    if name == "Set" {
                        if let Value::Map(m) = &v {
                            if m.borrow().is_empty() {
                                v = Value::Set(Rc::new(RefCell::new(Vec::new())));
                            }
                        }
                    }
                }
                self.bind_pattern(pattern, v, env, *mutable)?;
                Ok(())
            }
            Stmt::Expr(e) => {
                self.eval(e, env)?;
                Ok(())
            }
            Stmt::Return(e, _) => {
                let v = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                Err(Signal::Return(v))
            }
            Stmt::Break(e, _) => {
                let v = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                Err(Signal::Break(v))
            }
            Stmt::Continue(_) => Err(Signal::Continue),
            Stmt::Item(item) => {
                match item {
                    Item::Fn(f) => define(env, &f.name, Value::Function(Rc::new(f.clone())), false),
                    Item::Const(c) => {
                        let v = self.eval(&c.value, env)?;
                        define(env, &c.name, v, false);
                    }
                    _ => {}
                }
                Ok(())
            }
        }
    }

    /// Irrefutable binding (let / params / for). Refutable matching is in `try_match`.
    pub(super) fn bind_pattern(
        &mut self,
        pat: &Pattern,
        value: Value,
        env: &Env,
        mutable: bool,
    ) -> R<()> {
        match pat {
            Pattern::Binding(name) => {
                define(env, name, value, mutable);
                Ok(())
            }
            Pattern::Wildcard => Ok(()),
            Pattern::Tuple(parts) => {
                let items = self.as_seq(&value, Pos::default())?;
                if items.len() != parts.len() {
                    return rt(Pos::default(), "tuple pattern arity mismatch");
                }
                for (p, v) in parts.iter().zip(items) {
                    self.bind_pattern(p, v, env, mutable)?;
                }
                Ok(())
            }
            Pattern::List { items, rest } => {
                let seq = self.as_seq(&value, Pos::default())?;
                for (idx, p) in items.iter().enumerate() {
                    let v = seq.get(idx).cloned().unwrap_or(Value::Nil);
                    self.bind_pattern(p, v, env, mutable)?;
                }
                if let Some(name) = rest {
                    let tail: Vec<Value> = seq.iter().skip(items.len()).cloned().collect();
                    if !name.is_empty() {
                        define(env, name, list_val(tail), mutable);
                    }
                }
                Ok(())
            }
            Pattern::Struct { fields, .. } => {
                if let Value::Struct { fields: fmap, .. } = &value {
                    let fm = fmap.borrow();
                    for f in fields {
                        let v = fm.get(f).cloned().unwrap_or(Value::Nil);
                        define(env, f, v, mutable);
                    }
                    Ok(())
                } else {
                    rt(Pos::default(), "struct pattern on non-struct value")
                }
            }
            Pattern::Variant { path, args } => {
                if let Value::Enum { data, .. } = &value {
                    for (idx, p) in args.iter().enumerate() {
                        let v = data.get(idx).cloned().unwrap_or(Value::Nil);
                        self.bind_pattern(p, v, env, mutable)?;
                    }
                    Ok(())
                } else {
                    let _ = path;
                    rt(Pos::default(), "variant pattern on non-enum value")
                }
            }
            _ => rt(Pos::default(), "this pattern is only valid in a match arm"),
        }
    }
}

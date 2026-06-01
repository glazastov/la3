//! Interpreter: `match` evaluation and refutable pattern matching
//! (`try_match`). Split out of `interp.rs`.

use super::*;

impl Interp {
    pub(super) fn eval_match(
        &mut self,
        scrut: &Expr,
        arms: &[MatchArm],
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        let v = self.eval(scrut, env)?;
        for arm in arms {
            let menv = new_scope(Some(env.clone()));
            if self.try_match(&arm.pattern, &v, &menv)? {
                if let Some(guard) = &arm.guard {
                    let g = self.eval(guard, &menv)?;
                    if !self.as_bool(&g, pos)? {
                        continue;
                    }
                }
                return self.eval(&arm.body, &menv);
            }
        }
        rt(
            pos,
            "no match arm matched (match is meant to be exhaustive)",
        )
    }

    pub(super) fn try_match(&mut self, pat: &Pattern, value: &Value, env: &Env) -> R<bool> {
        match pat {
            Pattern::Wildcard => Ok(true),
            Pattern::Binding(name) => {
                define(env, name, value.clone(), false);
                Ok(true)
            }
            Pattern::Int(n) => Ok(matches!(value, Value::Int(m) if m == n)),
            Pattern::Bool(b) => Ok(matches!(value, Value::Bool(m) if m == b)),
            Pattern::Char(c) => Ok(matches!(value, Value::Char(m) if m == c)),
            Pattern::Str(s) => Ok(matches!(value, Value::Str(m) if m.as_str() == s)),
            Pattern::Nil => Ok(matches!(value, Value::Nil)),
            Pattern::Range { lo, hi, inclusive } => {
                if let Value::Int(n) = value {
                    Ok(if *inclusive {
                        n >= lo && n <= hi
                    } else {
                        n >= lo && n < hi
                    })
                } else {
                    Ok(false)
                }
            }
            Pattern::At(name, sub) => {
                if self.try_match(sub, value, env)? {
                    define(env, name, value.clone(), false);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Pattern::Or(parts) => {
                for p in parts {
                    if self.try_match(p, value, env)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Pattern::Tuple(parts) => {
                let seq = match self.as_seq(value, Pos::default()) {
                    Ok(s) => s,
                    Err(_) => return Ok(false),
                };
                if seq.len() != parts.len() {
                    return Ok(false);
                }
                for (p, v) in parts.iter().zip(seq) {
                    if !self.try_match(p, &v, env)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Pattern::List { items, rest } => {
                let seq = match value {
                    Value::List(l) => l.borrow().clone(),
                    _ => return Ok(false),
                };
                if rest.is_none() && seq.len() != items.len() {
                    return Ok(false);
                }
                if seq.len() < items.len() {
                    return Ok(false);
                }
                for (p, v) in items.iter().zip(seq.iter()) {
                    if !self.try_match(p, v, env)? {
                        return Ok(false);
                    }
                }
                if let Some(name) = rest {
                    if !name.is_empty() {
                        let tail: Vec<Value> = seq.iter().skip(items.len()).cloned().collect();
                        define(env, name, list_val(tail), false);
                    }
                }
                Ok(true)
            }
            Pattern::Variant { path, args } => {
                let want = path.last().unwrap();
                // `None` matches Nil.
                if want == "None" {
                    return Ok(matches!(value, Value::Nil));
                }
                if let Value::Enum { variant, data, .. } = value {
                    if variant.as_str() != want {
                        return Ok(false);
                    }
                    if !args.is_empty() {
                        if data.len() < args.len() {
                            return Ok(false);
                        }
                        for (p, v) in args.iter().zip(data.iter()) {
                            if !self.try_match(p, v, env)? {
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Pattern::Struct { fields, .. } => {
                if let Value::Struct { fields: fmap, .. } = value {
                    let fm = fmap.borrow();
                    for f in fields {
                        let v = fm.get(f).cloned().unwrap_or(Value::Nil);
                        define(env, f, v, false);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Pattern::Typed { binding, ty } => {
                if type_matches(value, ty) {
                    define(env, binding, value.clone(), false);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
}

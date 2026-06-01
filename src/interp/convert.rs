//! Interpreter: f-string evaluation, scalar coercions (`as_bool`/`as_int`/
//! ...), and `as` casts. Split out of `interp.rs`.

use super::*;

impl Interp {
    // ---- f-strings ----

    pub(super) fn eval_fstring(&mut self, parts: &[FStrPart], env: &Env, pos: Pos) -> R<Value> {
        let mut out = String::new();
        for part in parts {
            match part {
                FStrPart::Lit(s) => out.push_str(s),
                FStrPart::Expr { expr, spec } => {
                    let v = self.eval(expr, env)?;
                    out.push_str(&format_value(&v, spec.as_deref(), pos)?);
                }
            }
        }
        Ok(str_val(out))
    }

    // ---- conversions / helpers ----

    pub(super) fn as_bool(&self, v: &Value, pos: Pos) -> R<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Ref(cell, _) => self.as_bool(&cell.borrow(), pos),
            _ => rt(pos, format!("expected bool, found {}", v.type_name())),
        }
    }

    pub(super) fn as_int(&self, v: &Value, pos: Pos) -> R<i64> {
        match v {
            Value::Int(n) => Ok(*n),
            Value::Ref(cell, _) => self.as_int(&cell.borrow(), pos),
            _ => rt(pos, format!("expected integer, found {}", v.type_name())),
        }
    }

    pub(super) fn as_f64(&self, v: &Value, pos: Pos) -> R<f64> {
        match v {
            Value::Int(n) => Ok(*n as f64),
            Value::Float(f) => Ok(*f),
            Value::Ref(cell, _) => self.as_f64(&cell.borrow(), pos),
            _ => rt(pos, format!("expected number, found {}", v.type_name())),
        }
    }

    pub(super) fn as_seq(&self, v: &Value, pos: Pos) -> R<Vec<Value>> {
        match v {
            Value::Tuple(t) => Ok((**t).clone()),
            Value::List(l) => Ok(l.borrow().clone()),
            _ => rt(
                pos,
                format!("expected a tuple or list, found {}", v.type_name()),
            ),
        }
    }

    pub(super) fn len_of(&self, v: &Value, pos: Pos) -> R<usize> {
        match v {
            Value::Str(s) => Ok(s.len()),
            Value::List(l) => Ok(l.borrow().len()),
            Value::Set(s) => Ok(s.borrow().len()),
            Value::Map(m) => Ok(m.borrow().len()),
            Value::Tuple(t) => Ok(t.len()),
            Value::Channel(ch) => Ok(ch.borrow().buf.len()),
            _ => rt(pos, format!("{} has no length", v.type_name())),
        }
    }

    pub(super) fn cast(&self, v: Value, ty: &TypeExpr, pos: Pos) -> R<Value> {
        let name = if let TypeExpr::Named { name, .. } = ty {
            name.as_str()
        } else {
            ""
        };
        match name {
            "f64" | "f32" => Ok(Value::Float(self.as_f64(&v, pos)?)),
            "i8" | "i16" | "i32" | "i64" | "isize" => {
                let n = match v {
                    Value::Int(n) => n,
                    Value::Float(f) => f.trunc() as i64,
                    Value::Char(c) => c as i64,
                    _ => return rt(pos, "cannot cast to integer"),
                };
                Ok(Value::Int(mask_int(n, name)))
            }
            "u8" | "u16" | "u32" | "u64" | "usize" | "byte" => {
                let n = match v {
                    Value::Int(n) => n,
                    Value::Float(f) => f.trunc() as i64,
                    Value::Char(c) => c as i64,
                    _ => return rt(pos, "cannot cast to integer"),
                };
                Ok(Value::Int(mask_uint(n, name)))
            }
            "char" => match v {
                Value::Int(n) => char::from_u32(n as u32).map(Value::Char).ok_or_else(|| {
                    Signal::Error(Diagnostic::new(Phase::Runtime, pos, "invalid char code"))
                }),
                Value::Char(c) => Ok(Value::Char(c)),
                _ => rt(pos, "cannot cast to char"),
            },
            "str" => Ok(str_val(display(&v))),
            _ => Ok(v),
        }
    }
}

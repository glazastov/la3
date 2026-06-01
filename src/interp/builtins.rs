//! Interpreter: built-in free functions, stdlib module calls, and built-in
//! methods on primitives and collections. Split out of `interp.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;

use super::*;

impl Interp {
    // ---- builtins ----

    pub(super) fn try_builtin_fn(
        &mut self,
        name: &str,
        args: &[Expr],
        env: &Env,
        pos: Pos,
    ) -> R<Option<Value>> {
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval(a, env)?);
        }
        let v = match name {
            "str" => Some(str_val(display(argv.get(0).unwrap_or(&Value::Unit)))),
            "len" => Some(Value::Int(
                self.len_of(argv.get(0).unwrap_or(&Value::Unit), pos)? as i64,
            )),
            "print" => {
                print!("{}", display(argv.get(0).unwrap_or(&Value::Unit)));
                Some(Value::Unit)
            }
            "println" => {
                println!("{}", display(argv.get(0).unwrap_or(&Value::Unit)));
                Some(Value::Unit)
            }
            "idiv" => {
                let a = self.as_int(&argv[0], pos)?;
                let b = self.as_int(&argv[1], pos)?;
                if b == 0 {
                    return rt(pos, "floor division by zero");
                }
                Some(Value::Int(a.div_euclid(b)))
            }
            "min" => Some(self.fold_minmax(&argv, true, pos)?),
            "max" => Some(self.fold_minmax(&argv, false, pos)?),
            "abs" => match argv.get(0) {
                Some(Value::Int(n)) => Some(Value::Int(n.abs())),
                Some(Value::Float(f)) => Some(Value::Float(f.abs())),
                _ => return rt(pos, "abs expects a number"),
            },
            // Heap allocation (Section 11): `alloc(n)` returns a `*mut u8` to a
            // zeroed region of n bytes; `dealloc(p, n)` releases it.
            "alloc" => {
                let n = self
                    .as_int(argv.get(0).unwrap_or(&Value::Unit), pos)?
                    .max(0) as usize;
                let store = Rc::new(RefCell::new(vec![Value::Int(0); n]));
                Some(self.make_ptr(store, 0, 1, true))
            }
            "dealloc" => {
                // Model the free: drop the region so a later access surfaces as
                // an out-of-bounds dereference rather than reading stale data.
                if let Some(Value::Ptr(p)) = argv.get(0) {
                    p.store.borrow_mut().clear();
                }
                Some(Value::Unit)
            }
            // A buffered channel; capacity (if given) is advisory in v0.1.
            "channel" => {
                let capacity = match argv.get(0) {
                    Some(Value::Int(n)) if *n >= 0 => Some(*n as usize),
                    _ => None,
                };
                Some(Value::Channel(Rc::new(RefCell::new(ChannelData {
                    buf: VecDeque::new(),
                    closed: false,
                    capacity,
                }))))
            }
            // `all` resolves every future and returns their results in order.
            "all" => {
                let items = self.as_seq(&argv[0], pos).unwrap_or_default();
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.force(it, pos)?);
                }
                Some(list_val(out))
            }
            // `race` resolves with the first future to complete. The cooperative
            // scheduler runs to completion, so "first" is the first argument.
            "race" => {
                let first = argv.into_iter().next().unwrap_or(Value::Nil);
                let first = match first {
                    // `race(list)` and `race(a, b)` both reduce to a first item.
                    Value::List(l) => l.borrow().first().cloned().unwrap_or(Value::Nil),
                    other => other,
                };
                Some(self.force(first, pos)?)
            }
            "to_hex" => {
                let seq = self.as_seq(argv.get(0).unwrap_or(&Value::Unit), pos)?;
                let mut s = String::new();
                for b in seq {
                    let n = self.as_int(&b, pos)?;
                    let _ = write!(s, "{:02x}", (n & 0xff) as u8);
                }
                Some(str_val(s))
            }
            "from_hex" => {
                let s = match argv.get(0) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => return rt(pos, "from_hex expects a string"),
                };
                let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
                if clean.len() % 2 != 0 {
                    Some(err(str_val("odd-length hex string")))
                } else {
                    let mut out = Vec::new();
                    let mut okv = true;
                    let bytes: Vec<char> = clean.chars().collect();
                    let mut i = 0;
                    while i < bytes.len() {
                        let pair: String = bytes[i..i + 2].iter().collect();
                        match i64::from_str_radix(&pair, 16) {
                            Ok(n) => out.push(Value::Int(n)),
                            Err(_) => {
                                okv = false;
                                break;
                            }
                        }
                        i += 2;
                    }
                    Some(if okv {
                        ok(list_val(out))
                    } else {
                        err(str_val("invalid hex"))
                    })
                }
            }
            _ => None,
        };
        Ok(v)
    }

    pub(super) fn fold_minmax(&self, args: &[Value], want_min: bool, pos: Pos) -> R<Value> {
        if args.is_empty() {
            return rt(pos, "min/max needs at least one argument");
        }
        let mut best = args[0].clone();
        for v in &args[1..] {
            let take = match (&best, v) {
                (Value::Int(a), Value::Int(b)) => {
                    if want_min {
                        b < a
                    } else {
                        b > a
                    }
                }
                (Value::Float(a), Value::Float(b)) => {
                    if want_min {
                        b < a
                    } else {
                        b > a
                    }
                }
                _ => return rt(pos, "min/max expects numbers of the same type"),
            };
            if take {
                best = v.clone();
            }
        }
        Ok(best)
    }

    pub(super) fn call_module(
        &mut self,
        module: &str,
        func: &str,
        args: Vec<Value>,
        pos: Pos,
    ) -> R<Value> {
        let a0 = || args.get(0).cloned().unwrap_or(Value::Unit);
        match (module, func) {
            ("io", "print") => {
                print!("{}", display(&a0()));
                Ok(Value::Unit)
            }
            ("io", "println") => {
                println!("{}", display(&a0()));
                Ok(Value::Unit)
            }
            ("io", "eprintln") => {
                eprintln!("{}", display(&a0()));
                Ok(Value::Unit)
            }
            ("math", "sqrt") => Ok(Value::Float(self.as_f64(&a0(), pos)?.sqrt())),
            ("math", "floor") => Ok(Value::Float(self.as_f64(&a0(), pos)?.floor())),
            ("math", "ceil") => Ok(Value::Float(self.as_f64(&a0(), pos)?.ceil())),
            ("math", "round") => Ok(Value::Float(self.as_f64(&a0(), pos)?.round())),
            ("math", "abs") => Ok(Value::Float(self.as_f64(&a0(), pos)?.abs())),
            ("math", "log") => Ok(Value::Float(self.as_f64(&a0(), pos)?.ln())),
            ("math", "log2") => Ok(Value::Float(self.as_f64(&a0(), pos)?.log2())),
            ("math", "sin") => Ok(Value::Float(self.as_f64(&a0(), pos)?.sin())),
            ("math", "cos") => Ok(Value::Float(self.as_f64(&a0(), pos)?.cos())),
            ("os", "exit") => {
                let code = self.as_int(&a0(), pos).unwrap_or(0);
                std::process::exit(code as i32);
            }
            ("os", "args") => Ok(list_val(
                self.args.iter().map(|a| str_val(a.clone())).collect(),
            )),
            ("os", "env") => {
                let key = display(&a0());
                Ok(std::env::var(&key)
                    .map(str_val)
                    .map(some)
                    .unwrap_or(Value::Nil))
            }
            ("fs", "read") => {
                let path = display(&a0());
                match std::fs::read_to_string(&path) {
                    Ok(s) => Ok(ok(str_val(s))),
                    Err(e) => Ok(err(str_val(format!("{}: {}", path, e)))),
                }
            }
            ("fs", "write") => {
                let path = display(&a0());
                let content = display(&args.get(1).cloned().unwrap_or(Value::Unit));
                match std::fs::write(&path, content) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(str_val(format!("{}: {}", path, e)))),
                }
            }
            ("json", "encode") => Ok(ok(str_val(json_encode(&a0(), false, 0)))),
            ("json", "pretty") => Ok(ok(str_val(json_encode(&a0(), true, 0)))),
            ("json", "decode") => {
                let s = display(&a0());
                match json_decode(&s) {
                    Ok(v) => Ok(ok(v)),
                    Err(e) => Ok(err(str_val(e))),
                }
            }
            ("bytes", "to_hex") => {
                let seq = self.as_seq(&a0(), pos)?;
                let mut s = String::new();
                for b in seq {
                    let n = self.as_int(&b, pos)?;
                    let _ = write!(s, "{:02x}", (n & 0xff) as u8);
                }
                Ok(str_val(s))
            }
            _ => rt(pos, format!("unknown stdlib function {}.{}", module, func)),
        }
    }

    pub(super) fn builtin_method(
        &mut self,
        recv: Value,
        method: &str,
        args: Vec<Value>,
        _env: &Env,
        pos: Pos,
    ) -> R<Value> {
        match &recv {
            // ---- universal ----
            _ if method == "len" => Ok(Value::Int(self.len_of(&recv, pos)? as i64)),
            _ => self.builtin_method_inner(recv, method, args, pos),
        }
    }

    pub(super) fn builtin_method_inner(
        &mut self,
        recv: Value,
        method: &str,
        args: Vec<Value>,
        pos: Pos,
    ) -> R<Value> {
        match recv {
            // ---- Channel (Section 12) ----
            Value::Channel(ch) => match method {
                "send" => {
                    let v = args.into_iter().next().unwrap_or(Value::Unit);
                    let mut c = ch.borrow_mut();
                    if c.closed {
                        return rt(pos, "send on a closed channel");
                    }
                    c.buf.push_back(v);
                    Ok(Value::Unit)
                }
                "recv" => match self.channel_recv(&ch, pos)? {
                    Some(v) => Ok(some(v)),
                    None => Ok(Value::Nil),
                },
                "close" => {
                    ch.borrow_mut().closed = true;
                    Ok(Value::Unit)
                }
                "is_closed" => Ok(Value::Bool(ch.borrow().closed)),
                "is_empty" => Ok(Value::Bool(ch.borrow().buf.is_empty())),
                "capacity" => Ok(ch
                    .borrow()
                    .capacity
                    .map(|c| Value::Int(c as i64))
                    .unwrap_or(Value::Nil)),
                _ => rt(pos, format!("Channel has no method '{}'", method)),
            },
            // ---- Future / spawned task ----
            Value::Future(task) => match method {
                "join" | "await" => self.run_task(&task, pos),
                _ => rt(pos, format!("Future has no method '{}'", method)),
            },
            // ---- Option / Result (Enum) and None (Nil) ----
            Value::Nil => match method {
                "is_some" | "is_ok" => Ok(Value::Bool(false)),
                "is_none" | "is_nil" => Ok(Value::Bool(true)),
                "unwrap" => rt(pos, "called unwrap on None"),
                "unwrap_or" => Ok(args.into_iter().next().unwrap_or(Value::Nil)),
                "unwrap_or_else" => {
                    let f = args.into_iter().next().unwrap_or(Value::Nil);
                    self.call_value(f, vec![Value::Nil], pos)
                }
                "map" | "and_then" => Ok(Value::Nil),
                "map_err" => Ok(Value::Nil),
                _ => rt(pos, format!("nil has no method '{}'", method)),
            },
            Value::Enum {
                ref ty,
                ref variant,
                ref data,
            } if ty.as_str() == "Option" || ty.as_str() == "Result" => {
                let inner = data.get(0).cloned();
                match method {
                    "is_some" => Ok(Value::Bool(variant.as_str() == "Some")),
                    "is_none" => Ok(Value::Bool(variant.as_str() == "None")),
                    "is_ok" => Ok(Value::Bool(variant.as_str() == "Ok")),
                    "is_err" => Ok(Value::Bool(variant.as_str() == "Err")),
                    "unwrap" => match variant.as_str() {
                        "Some" | "Ok" => Ok(inner.unwrap_or(Value::Unit)),
                        _ => rt(pos, format!("called unwrap on {}", variant)),
                    },
                    "unwrap_err" => match variant.as_str() {
                        "Err" => Ok(inner.unwrap_or(Value::Unit)),
                        _ => rt(pos, "called unwrap_err on non-Err"),
                    },
                    "unwrap_or" => match variant.as_str() {
                        "Some" | "Ok" => Ok(inner.unwrap_or(Value::Unit)),
                        _ => Ok(args.into_iter().next().unwrap_or(Value::Nil)),
                    },
                    "unwrap_or_else" => match variant.as_str() {
                        "Some" | "Ok" => Ok(inner.unwrap_or(Value::Unit)),
                        _ => {
                            let f = args.into_iter().next().unwrap_or(Value::Nil);
                            self.call_value(f, vec![inner.unwrap_or(Value::Nil)], pos)
                        }
                    },
                    "map" => match variant.as_str() {
                        "Some" => {
                            let f = args.into_iter().next().unwrap();
                            Ok(some(self.call_value(f, vec![inner.unwrap()], pos)?))
                        }
                        "Ok" => {
                            let f = args.into_iter().next().unwrap();
                            Ok(ok(self.call_value(f, vec![inner.unwrap()], pos)?))
                        }
                        _ => Ok(recv.clone()),
                    },
                    "map_err" => match variant.as_str() {
                        "Err" => {
                            let f = args.into_iter().next().unwrap();
                            Ok(err(self.call_value(f, vec![inner.unwrap()], pos)?))
                        }
                        _ => Ok(recv.clone()),
                    },
                    "and_then" => match variant.as_str() {
                        "Some" | "Ok" => {
                            let f = args.into_iter().next().unwrap();
                            self.call_value(f, vec![inner.unwrap()], pos)
                        }
                        _ => Ok(recv.clone()),
                    },
                    _ => rt(pos, format!("{} has no method '{}'", ty, method)),
                }
            }

            // ---- strings ----
            Value::Str(ref s) => self.str_method(s, method, args, pos),

            // ---- lists ----
            Value::List(ref l) => self.list_method(l.clone(), method, args, pos),

            // ---- sets ----
            Value::Set(ref s) => match method {
                "insert" => {
                    let v = args.into_iter().next().unwrap_or(Value::Nil);
                    let mut b = s.borrow_mut();
                    if !b.iter().any(|e| value_eq(e, &v)) {
                        b.push(v);
                    }
                    Ok(Value::Unit)
                }
                "contains" => {
                    let v = args.into_iter().next().unwrap_or(Value::Nil);
                    Ok(Value::Bool(s.borrow().iter().any(|e| value_eq(e, &v))))
                }
                "remove" => {
                    let v = args.into_iter().next().unwrap_or(Value::Nil);
                    s.borrow_mut().retain(|e| !value_eq(e, &v));
                    Ok(Value::Unit)
                }
                "is_empty" => Ok(Value::Bool(s.borrow().is_empty())),
                _ => rt(pos, format!("Set has no method '{}'", method)),
            },

            // ---- maps ----
            Value::Map(ref m) => match method {
                "get" => {
                    let k = args.into_iter().next().unwrap_or(Value::Nil);
                    let found = m
                        .borrow()
                        .iter()
                        .find(|(kk, _)| value_eq(kk, &k))
                        .map(|(_, v)| v.clone());
                    Ok(found.map(some).unwrap_or(Value::Nil))
                }
                "contains" | "contains_key" => {
                    let k = args.into_iter().next().unwrap_or(Value::Nil);
                    Ok(Value::Bool(
                        m.borrow().iter().any(|(kk, _)| value_eq(kk, &k)),
                    ))
                }
                "remove" => {
                    let k = args.into_iter().next().unwrap_or(Value::Nil);
                    m.borrow_mut().retain(|(kk, _)| !value_eq(kk, &k));
                    Ok(Value::Unit)
                }
                "keys" => Ok(list_val(
                    m.borrow().iter().map(|(k, _)| k.clone()).collect(),
                )),
                "values" => Ok(list_val(
                    m.borrow().iter().map(|(_, v)| v.clone()).collect(),
                )),
                "is_empty" => Ok(Value::Bool(m.borrow().is_empty())),
                _ => rt(pos, format!("Map has no method '{}'", method)),
            },

            // ---- numbers ----
            Value::Int(n) => match method {
                "to_str" => Ok(str_val(n.to_string())),
                "abs" => Ok(Value::Int(n.abs())),
                _ => rt(pos, format!("int has no method '{}'", method)),
            },
            Value::Float(f) => match method {
                "floor" => Ok(Value::Float(f.floor())),
                "ceil" => Ok(Value::Float(f.ceil())),
                "round" => Ok(Value::Float(f.round())),
                "to_str" => Ok(str_val(format!("{}", f))),
                _ => rt(pos, format!("float has no method '{}'", method)),
            },

            other => rt(
                pos,
                format!("type {} has no method '{}'", other.type_name(), method),
            ),
        }
    }

    pub(super) fn str_method(
        &mut self,
        s: &str,
        method: &str,
        args: Vec<Value>,
        pos: Pos,
    ) -> R<Value> {
        let arg_str = |i: usize| -> String {
            match args.get(i) {
                Some(Value::Str(s)) => s.to_string(),
                Some(v) => display(v),
                None => String::new(),
            }
        };
        match method {
            "is_empty" => Ok(Value::Bool(s.is_empty())),
            "to_upper" => Ok(str_val(s.to_uppercase())),
            "to_lower" => Ok(str_val(s.to_lowercase())),
            "trim" => Ok(str_val(s.trim().to_string())),
            "trim_start" => Ok(str_val(s.trim_start().to_string())),
            "trim_end" => Ok(str_val(s.trim_end().to_string())),
            "contains" => Ok(Value::Bool(s.contains(&arg_str(0)))),
            "starts_with" => Ok(Value::Bool(s.starts_with(&arg_str(0)))),
            "ends_with" => Ok(Value::Bool(s.ends_with(&arg_str(0)))),
            "replace" => Ok(str_val(s.replace(&arg_str(0), &arg_str(1)))),
            "repeat" => {
                let n = self.as_int(args.get(0).unwrap_or(&Value::Int(0)), pos)?;
                Ok(str_val(s.repeat(n.max(0) as usize)))
            }
            "split" => {
                let sep = arg_str(0);
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| str_val(c.to_string())).collect()
                } else {
                    s.split(&sep).map(str_val).collect()
                };
                Ok(list_val(parts))
            }
            "split_once" => {
                let sep = arg_str(0);
                match s.split_once(&sep) {
                    Some((a, b)) => Ok(some(Value::Tuple(Rc::new(vec![str_val(a), str_val(b)])))),
                    None => Ok(Value::Nil),
                }
            }
            "chars" => Ok(list_val(s.chars().map(Value::Char).collect())),
            "parse" => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    Ok(ok(Value::Int(n)))
                } else if let Ok(f) = s.trim().parse::<f64>() {
                    Ok(ok(Value::Float(f)))
                } else {
                    Ok(err(str_val(format!("cannot parse '{}'", s))))
                }
            }
            _ => rt(pos, format!("str has no method '{}'", method)),
        }
    }

    pub(super) fn list_method(
        &mut self,
        l: ListRef,
        method: &str,
        args: Vec<Value>,
        pos: Pos,
    ) -> R<Value> {
        match method {
            "push" | "append" => {
                l.borrow_mut()
                    .push(args.into_iter().next().unwrap_or(Value::Nil));
                Ok(Value::Unit)
            }
            "pop" => Ok(l.borrow_mut().pop().map(some).unwrap_or(Value::Nil)),
            "first" => Ok(l.borrow().first().cloned().map(some).unwrap_or(Value::Nil)),
            "last" => Ok(l.borrow().last().cloned().map(some).unwrap_or(Value::Nil)),
            "is_empty" => Ok(Value::Bool(l.borrow().is_empty())),
            "contains" => {
                let v = args.into_iter().next().unwrap_or(Value::Nil);
                Ok(Value::Bool(l.borrow().iter().any(|e| value_eq(e, &v))))
            }
            "extend" => {
                let other = args.into_iter().next().unwrap_or(Value::Nil);
                let items = self.as_seq(&other, pos)?;
                l.borrow_mut().extend(items);
                Ok(Value::Unit)
            }
            "map" => {
                let f = args.into_iter().next().unwrap();
                let snapshot = l.borrow().clone();
                let mut out = Vec::with_capacity(snapshot.len());
                for v in snapshot {
                    out.push(self.call_value(f.clone(), vec![v], pos)?);
                }
                Ok(list_val(out))
            }
            "filter" => {
                let f = args.into_iter().next().unwrap();
                let snapshot = l.borrow().clone();
                let mut out = Vec::new();
                for v in snapshot {
                    let keep = self.call_value(f.clone(), vec![v.clone()], pos)?;
                    if self.as_bool(&keep, pos)? {
                        out.push(v);
                    }
                }
                Ok(list_val(out))
            }
            "reduce" => {
                let mut acc = args.get(0).cloned().unwrap_or(Value::Nil);
                let f = args.get(1).cloned().unwrap();
                let snapshot = l.borrow().clone();
                for v in snapshot {
                    acc = self.call_value(f.clone(), vec![acc, v], pos)?;
                }
                Ok(acc)
            }
            "sort_by" => {
                let f = args.into_iter().next().unwrap();
                let mut snapshot = l.borrow().clone();
                // insertion sort using the comparator (returns true if a precedes b)
                for i in 1..snapshot.len() {
                    let mut j = i;
                    while j > 0 {
                        let before = self.call_value(
                            f.clone(),
                            vec![snapshot[j].clone(), snapshot[j - 1].clone()],
                            pos,
                        )?;
                        if self.as_bool(&before, pos)? {
                            snapshot.swap(j, j - 1);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                }
                Ok(list_val(snapshot))
            }
            "enumerate" => {
                let out: Vec<Value> = l
                    .borrow()
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                    .collect();
                Ok(list_val(out))
            }
            "zip" => {
                let other = args.into_iter().next().unwrap_or(Value::Nil);
                let b = self.as_seq(&other, pos)?;
                let a = l.borrow();
                let out: Vec<Value> = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| Value::Tuple(Rc::new(vec![x.clone(), y.clone()])))
                    .collect();
                Ok(list_val(out))
            }
            "group_by" => {
                let f = args.into_iter().next().unwrap();
                let snapshot = l.borrow().clone();
                let mut map: Vec<(Value, Value)> = Vec::new();
                for v in snapshot {
                    let key = self.call_value(f.clone(), vec![v.clone()], pos)?;
                    if let Some(slot) = map.iter_mut().find(|(k, _)| value_eq(k, &key)) {
                        if let Value::List(inner) = &slot.1 {
                            inner.borrow_mut().push(v);
                        }
                    } else {
                        map.push((key, list_val(vec![v])));
                    }
                }
                Ok(Value::Map(Rc::new(RefCell::new(map))))
            }
            "collect" => {
                // Collecting a list of pairs builds a map; otherwise identity.
                let b = l.borrow();
                if b.iter()
                    .all(|v| matches!(v, Value::Tuple(t) if t.len() == 2))
                    && !b.is_empty()
                {
                    let entries: Vec<(Value, Value)> = b
                        .iter()
                        .map(|v| {
                            if let Value::Tuple(t) = v {
                                (t[0].clone(), t[1].clone())
                            } else {
                                unreachable!()
                            }
                        })
                        .collect();
                    Ok(Value::Map(Rc::new(RefCell::new(entries))))
                } else {
                    Ok(Value::List(l.clone()))
                }
            }
            "sum" => {
                let b = l.borrow();
                let mut acc = 0i64;
                let mut facc = 0f64;
                let mut is_float = false;
                for v in b.iter() {
                    match v {
                        Value::Int(n) => acc += n,
                        Value::Float(f) => {
                            is_float = true;
                            facc += f;
                        }
                        _ => return rt(pos, "sum expects numbers"),
                    }
                }
                Ok(if is_float {
                    Value::Float(facc + acc as f64)
                } else {
                    Value::Int(acc)
                })
            }
            _ => rt(pos, format!("List has no method '{}'", method)),
        }
    }
}

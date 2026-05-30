//! Tree-walking interpreter for La3.
//!
//! Notable runtime decisions, all documented in the README:
//! - `nil` and `None` are the same runtime value ([`Value::Nil`]), exactly as the
//!   language spec states. `Some`/`Ok`/`Err` are tagged enum values.
//! - Closures capture their defining environment by reference; `move` is accepted
//!   but behaves the same in v0.1.
//! - `spawn`, `await`, `all`, and `race` run synchronously (the interpreter is
//!   single-threaded), which preserves observable results for example programs.
//! - Floor division is the `idiv(a, b)` builtin (the `//` syntax is a comment).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos, Result as DResult};

// ---- values ----

type ListRef = Rc<RefCell<Vec<Value>>>;
type MapRef = Rc<RefCell<Vec<(Value, Value)>>>;
type FieldsRef = Rc<RefCell<HashMap<String, Value>>>;

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(Rc<String>),
    List(ListRef),
    Map(MapRef),
    Set(ListRef),
    Tuple(Rc<Vec<Value>>),
    Range { start: i64, end: i64, inclusive: bool },
    Struct { name: Rc<String>, fields: FieldsRef },
    Enum { ty: Rc<String>, variant: Rc<String>, data: Rc<Vec<Value>> },
    Closure(Rc<ClosureData>),
    Function(Rc<FnDecl>),
    Unit,
}

pub struct ClosureData {
    params: Vec<Param>,
    body: Expr,
    env: Env,
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Value::Nil => "nil".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Char(_) => "char".into(),
            Value::Str(_) => "str".into(),
            Value::List(_) => "List".into(),
            Value::Map(_) => "Map".into(),
            Value::Set(_) => "Set".into(),
            Value::Tuple(_) => "tuple".into(),
            Value::Range { .. } => "Range".into(),
            Value::Struct { name, .. } => name.to_string(),
            Value::Enum { ty, .. } => ty.to_string(),
            Value::Closure(_) | Value::Function(_) => "fn".into(),
            Value::Unit => "()".into(),
        }
    }
}

fn some(v: Value) -> Value {
    Value::Enum { ty: Rc::new("Option".into()), variant: Rc::new("Some".into()), data: Rc::new(vec![v]) }
}
fn ok(v: Value) -> Value {
    Value::Enum { ty: Rc::new("Result".into()), variant: Rc::new("Ok".into()), data: Rc::new(vec![v]) }
}
fn err(v: Value) -> Value {
    Value::Enum { ty: Rc::new("Result".into()), variant: Rc::new("Err".into()), data: Rc::new(vec![v]) }
}
fn str_val(s: impl Into<String>) -> Value {
    Value::Str(Rc::new(s.into()))
}
fn list_val(v: Vec<Value>) -> Value {
    Value::List(Rc::new(RefCell::new(v)))
}

// ---- environment ----

struct Var {
    cell: Rc<RefCell<Value>>,
    mutable: bool,
}

pub struct Scope {
    vars: HashMap<String, Var>,
    parent: Option<Env>,
}

pub type Env = Rc<RefCell<Scope>>;

fn new_scope(parent: Option<Env>) -> Env {
    Rc::new(RefCell::new(Scope { vars: HashMap::new(), parent }))
}

fn lookup(env: &Env, name: &str) -> Option<Rc<RefCell<Value>>> {
    let scope = env.borrow();
    if let Some(v) = scope.vars.get(name) {
        return Some(v.cell.clone());
    }
    let parent = scope.parent.clone();
    drop(scope);
    parent.and_then(|p| lookup(&p, name))
}

fn lookup_var(env: &Env, name: &str) -> Option<(Rc<RefCell<Value>>, bool)> {
    let scope = env.borrow();
    if let Some(v) = scope.vars.get(name) {
        return Some((v.cell.clone(), v.mutable));
    }
    let parent = scope.parent.clone();
    drop(scope);
    parent.and_then(|p| lookup_var(&p, name))
}

fn define(env: &Env, name: &str, value: Value, mutable: bool) {
    env.borrow_mut().vars.insert(
        name.to_string(),
        Var { cell: Rc::new(RefCell::new(value)), mutable },
    );
}

// ---- control flow ----

pub enum Signal {
    Return(Value),
    Break(Value),
    Continue,
    Error(Diagnostic),
}

type R<T> = std::result::Result<T, Signal>;

fn rt<T>(pos: Pos, msg: impl Into<String>) -> R<T> {
    Err(Signal::Error(Diagnostic::new(Phase::Runtime, pos, msg)))
}

// ---- interpreter ----

pub struct Interp {
    globals: Env,
    structs: HashMap<String, StructDecl>,
    enums: HashMap<String, EnumDecl>,
    /// type name -> (method name -> decl)
    methods: HashMap<String, HashMap<String, Rc<FnDecl>>>,
    /// variant name -> enum name (for bare variant patterns)
    variant_owner: HashMap<String, String>,
    /// Program arguments exposed via `os.args()`.
    args: Vec<String>,
}

impl Interp {
    pub fn new() -> Self {
        Interp {
            globals: new_scope(None),
            structs: HashMap::new(),
            enums: HashMap::new(),
            methods: HashMap::new(),
            variant_owner: HashMap::new(),
            args: Vec::new(),
        }
    }

    /// Set the arguments returned by `os.args()`.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.args = args;
    }

    /// Load all items, then call `main` if present.
    pub fn run(&mut self, prog: &Program) -> DResult<()> {
        self.load(prog);
        if lookup(&self.globals, "main").is_some() {
            let main = lookup(&self.globals, "main").unwrap();
            let f = main.borrow().clone();
            match self.call_value(f, vec![], Pos::default()) {
                Ok(_) => Ok(()),
                Err(Signal::Error(d)) => Err(d),
                Err(_) => Ok(()),
            }
        } else {
            Ok(())
        }
    }

    fn load(&mut self, prog: &Program) {
        // Built-in enums so `Some`, `Ok`, `Err`, `None` resolve.
        self.variant_owner.insert("Some".into(), "Option".into());
        self.variant_owner.insert("None".into(), "Option".into());
        self.variant_owner.insert("Ok".into(), "Result".into());
        self.variant_owner.insert("Err".into(), "Result".into());

        for item in &prog.items {
            match item {
                Item::Fn(f) => {
                    define(&self.globals, &f.name, Value::Function(Rc::new(f.clone())), false);
                }
                Item::Struct(s) => {
                    self.structs.insert(s.name.clone(), s.clone());
                }
                Item::Enum(e) => {
                    for v in &e.variants {
                        self.variant_owner.insert(v.name.clone(), e.name.clone());
                    }
                    self.enums.insert(e.name.clone(), e.clone());
                }
                Item::Impl(b) => {
                    let entry = self.methods.entry(b.ty.clone()).or_default();
                    for m in &b.methods {
                        entry.insert(m.name.clone(), Rc::new(m.clone()));
                    }
                }
                Item::Const(c) => {
                    // Evaluate const in the global scope.
                    if let Ok(v) = self.eval(&c.value, &self.globals.clone()) {
                        define(&self.globals, &c.name, v, false);
                    }
                }
                Item::Use(_) | Item::TypeAlias { .. } | Item::Interface(_) => {}
            }
        }
    }

    // ---- statements / blocks ----

    fn eval_block(&mut self, block: &Block, parent: &Env) -> R<Value> {
        let env = new_scope(Some(parent.clone()));
        self.eval_block_in(block, &env)
    }

    fn eval_block_in(&mut self, block: &Block, env: &Env) -> R<Value> {
        for stmt in &block.stmts {
            self.eval_stmt(stmt, env)?;
        }
        if let Some(tail) = &block.tail {
            self.eval(tail, env)
        } else {
            Ok(Value::Unit)
        }
    }

    fn eval_stmt(&mut self, stmt: &Stmt, env: &Env) -> R<()> {
        match stmt {
            Stmt::Let { pattern, mutable, ty, value, .. } => {
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
    fn bind_pattern(&mut self, pat: &Pattern, value: Value, env: &Env, mutable: bool) -> R<()> {
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

    // ---- expressions ----

    fn eval(&mut self, expr: &Expr, env: &Env) -> R<Value> {
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
            ExprKind::Assign { target, op, value } => self.eval_assign(target, *op, value, env, pos),
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
            ExprKind::Range { start, end, inclusive } => {
                let s = self.eval(start, env)?;
                let e = self.eval(end, env)?;
                Ok(Value::Range {
                    start: self.as_int(&s, pos)?,
                    end: self.as_int(&e, pos)?,
                    inclusive: *inclusive,
                })
            }
            ExprKind::StructLit { name, fields, spread } => {
                self.eval_struct_lit(name, fields, spread, env, pos)
            }
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
            ExprKind::WhileLet { pattern, expr, body } => {
                self.eval_while_let(pattern, expr, body, env)
            }
            ExprKind::For { pattern, iter, body } => self.eval_for(pattern, iter, body, env, pos),
            ExprKind::Closure { params, body, .. } => Ok(Value::Closure(Rc::new(ClosureData {
                params: params.clone(),
                body: (**body).clone(),
                env: env.clone(),
            }))),
            ExprKind::Call { callee, args } => self.eval_call(callee, args, env, pos),
            ExprKind::MethodCall { recv, optional, method, args, .. } => {
                self.eval_method(recv, *optional, method, args, env, pos)
            }
            ExprKind::Field { recv, optional, name } => {
                self.eval_field(recv, *optional, name, env, pos)
            }
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
            ExprKind::Await(inner) => self.eval(inner, env),
            ExprKind::Spawn(body) => self.eval_block(body, env),
            ExprKind::Unsafe(body) => self.eval_block(body, env),
            ExprKind::TryCatch { body, catches, finally } => {
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

    fn read_ident(&mut self, name: &str, env: &Env, pos: Pos) -> R<Value> {
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

    fn eval_path(&mut self, segs: &[String], env: &Env, pos: Pos) -> R<Value> {
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

    fn eval_unary(&mut self, op: UnOp, expr: &Expr, env: &Env, pos: Pos) -> R<Value> {
        // `&`, `&mut`, `&raw`, `*` are pass-through in this value-based interpreter.
        match op {
            UnOp::Ref | UnOp::RefMut | UnOp::RawRef | UnOp::Deref => self.eval(expr, env),
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

    fn eval_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, env: &Env, pos: Pos) -> R<Value> {
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
        if matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr) {
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

    fn compare(&self, op: BinOp, l: &Value, r: &Value, pos: Pos) -> R<Value> {
        let ord = match (l, r) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.partial_cmp(b),
            (Value::Char(a), Value::Char(b)) => a.partial_cmp(b),
            _ => return rt(pos, format!("cannot compare {} and {}", l.type_name(), r.type_name())),
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

    fn eval_assign(
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
                let (cell, mutable) = lookup_var(env, name)
                    .ok_or_else(|| Signal::Error(Diagnostic::new(Phase::Runtime, pos, format!("undefined name '{}'", name))))?;
                if !mutable {
                    return rt(pos, format!("cannot assign to immutable binding '{}' (declare it `let mut`)", name));
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
            _ => rt(pos, "invalid assignment target"),
        }
    }

    fn apply_compound(&self, op: Option<BinOp>, cur: Value, rhs: Value, pos: Pos) -> R<Value> {
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
                (Value::Str(a), Value::Str(c)) if b == BinOp::Add => Ok(str_val(format!("{}{}", a, c))),
                _ => rt(pos, "type mismatch in compound assignment"),
            },
        }
    }

    fn eval_struct_lit(
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
        Ok(Value::Struct { name: Rc::new(name.to_string()), fields: Rc::new(RefCell::new(map)) })
    }

    fn eval_match(&mut self, scrut: &Expr, arms: &[MatchArm], env: &Env, pos: Pos) -> R<Value> {
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
        rt(pos, "no match arm matched (match is meant to be exhaustive)")
    }

    fn try_match(&mut self, pat: &Pattern, value: &Value, env: &Env) -> R<bool> {
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
                    Ok(if *inclusive { n >= lo && n <= hi } else { n >= lo && n < hi })
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

    fn eval_loop(&mut self, body: &Block, env: &Env) -> R<Value> {
        loop {
            match self.eval_block(body, env) {
                Ok(_) => {}
                Err(Signal::Break(v)) => return Ok(v),
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
    }

    fn eval_while(&mut self, cond: &Expr, body: &Block, env: &Env, pos: Pos) -> R<Value> {
        loop {
            let c = self.eval(cond, env)?;
            if !self.as_bool(&c, pos)? {
                break;
            }
            match self.eval_block(body, env) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    fn eval_while_let(&mut self, pat: &Pattern, expr: &Expr, body: &Block, env: &Env) -> R<Value> {
        loop {
            let v = self.eval(expr, env)?;
            let menv = new_scope(Some(env.clone()));
            if !self.try_match(pat, &v, &menv)? {
                break;
            }
            match self.eval_block_in(body, &new_scope(Some(menv))) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    fn eval_for(&mut self, pat: &Pattern, iter: &Expr, body: &Block, env: &Env, pos: Pos) -> R<Value> {
        let it = self.eval(iter, env)?;
        let items = self.iterate(&it, pos)?;
        for item in items {
            let loop_env = new_scope(Some(env.clone()));
            self.bind_pattern(pat, item, &loop_env, false)?;
            match self.eval_block_in(body, &new_scope(Some(loop_env))) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    fn iterate(&self, v: &Value, pos: Pos) -> R<Vec<Value>> {
        match v {
            Value::Range { start, end, inclusive } => {
                let mut out = Vec::new();
                let hi = if *inclusive { *end + 1 } else { *end };
                let mut i = *start;
                while i < hi {
                    out.push(Value::Int(i));
                    i += 1;
                }
                Ok(out)
            }
            Value::List(l) => Ok(l.borrow().clone()),
            Value::Set(s) => Ok(s.borrow().clone()),
            Value::Map(m) => Ok(m
                .borrow()
                .iter()
                .map(|(k, val)| Value::Tuple(Rc::new(vec![k.clone(), val.clone()])))
                .collect()),
            Value::Str(s) => Ok(s.chars().map(Value::Char).collect()),
            _ => rt(pos, format!("{} is not iterable", v.type_name())),
        }
    }

    // ---- calls ----

    fn eval_call(&mut self, callee: &Expr, args: &[Expr], env: &Env, pos: Pos) -> R<Value> {
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
            if segs.len() == 2 && (self.enums.contains_key(&segs[0]) || segs[0] == "Option" || segs[0] == "Result") {
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

    fn call_value(&mut self, f: Value, args: Vec<Value>, pos: Pos) -> R<Value> {
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

    fn call_fn(&mut self, decl: &FnDecl, self_val: Option<Value>, args: Vec<Value>, pos: Pos) -> R<Value> {
        let scope = new_scope(Some(self.globals.clone()));
        // variadic handling
        let fixed: Vec<&Param> = decl.params.iter().filter(|p| !p.is_self).collect();
        if decl.variadic.is_some() {
            let n = fixed.len();
            let (head, tail) = if args.len() >= n { args.split_at(n) } else { (&args[..], &[][..]) };
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

    fn eval_field(&mut self, recv: &Expr, optional: bool, name: &str, env: &Env, pos: Pos) -> R<Value> {
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
        let obj = self.eval(recv, env)?;
        if optional && matches!(obj, Value::Nil) {
            return Ok(Value::Nil);
        }
        match &obj {
            Value::Struct { fields, .. } => Ok(fields.borrow().get(name).cloned().unwrap_or(Value::Nil)),
            Value::Tuple(items) => {
                if let Ok(i) = name.parse::<usize>() {
                    Ok(items.get(i).cloned().unwrap_or(Value::Nil))
                } else {
                    rt(pos, "tuple fields are numeric")
                }
            }
            _ => rt(pos, format!("type {} has no field '{}'", obj.type_name(), name)),
        }
    }

    fn eval_method(
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
                        .unwrap_or(method == "Some" || method == "None" || method == "Ok" || method == "Err");
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

    fn find_method(&self, ty: &str, name: &str) -> Option<Rc<FnDecl>> {
        self.methods.get(ty).and_then(|m| m.get(name)).cloned()
    }

    fn eval_index(&mut self, recv: &Expr, index: &Expr, env: &Env, pos: Pos) -> R<Value> {
        let c = self.eval(recv, env)?;
        let i = self.eval(index, env)?;
        match &c {
            Value::List(l) => {
                if let Value::Range { start, end, inclusive } = i {
                    let b = l.borrow();
                    let hi = (if inclusive { end + 1 } else { end }).clamp(0, b.len() as i64) as usize;
                    let lo = start.clamp(0, b.len() as i64) as usize;
                    Ok(list_val(b[lo..hi.max(lo)].to_vec()))
                } else {
                    let idx = self.as_int(&i, pos)?;
                    let b = l.borrow();
                    b.get(idx as usize)
                        .cloned()
                        .ok_or_else(|| Signal::Error(Diagnostic::new(Phase::Runtime, pos, "list index out of bounds")))
                }
            }
            Value::Map(m) => {
                let b = m.borrow();
                b.iter()
                    .find(|(k, _)| value_eq(k, &i))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| Signal::Error(Diagnostic::new(Phase::Runtime, pos, "key not present in map")))
            }
            Value::Str(s) => {
                let idx = self.as_int(&i, pos)?;
                s.chars()
                    .nth(idx as usize)
                    .map(Value::Char)
                    .ok_or_else(|| Signal::Error(Diagnostic::new(Phase::Runtime, pos, "string index out of bounds")))
            }
            _ => rt(pos, format!("cannot index {}", c.type_name())),
        }
    }

    // ---- f-strings ----

    fn eval_fstring(&mut self, parts: &[FStrPart], env: &Env, pos: Pos) -> R<Value> {
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

    fn as_bool(&self, v: &Value, pos: Pos) -> R<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            _ => rt(pos, format!("expected bool, found {}", v.type_name())),
        }
    }

    fn as_int(&self, v: &Value, pos: Pos) -> R<i64> {
        match v {
            Value::Int(n) => Ok(*n),
            _ => rt(pos, format!("expected integer, found {}", v.type_name())),
        }
    }

    fn as_f64(&self, v: &Value, pos: Pos) -> R<f64> {
        match v {
            Value::Int(n) => Ok(*n as f64),
            Value::Float(f) => Ok(*f),
            _ => rt(pos, format!("expected number, found {}", v.type_name())),
        }
    }

    fn as_seq(&self, v: &Value, pos: Pos) -> R<Vec<Value>> {
        match v {
            Value::Tuple(t) => Ok((**t).clone()),
            Value::List(l) => Ok(l.borrow().clone()),
            _ => rt(pos, format!("expected a tuple or list, found {}", v.type_name())),
        }
    }

    fn len_of(&self, v: &Value, pos: Pos) -> R<usize> {
        match v {
            Value::Str(s) => Ok(s.len()),
            Value::List(l) => Ok(l.borrow().len()),
            Value::Set(s) => Ok(s.borrow().len()),
            Value::Map(m) => Ok(m.borrow().len()),
            Value::Tuple(t) => Ok(t.len()),
            _ => rt(pos, format!("{} has no length", v.type_name())),
        }
    }

    fn cast(&self, v: Value, ty: &TypeExpr, pos: Pos) -> R<Value> {
        let name = if let TypeExpr::Named { name, .. } = ty { name.as_str() } else { "" };
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
                Value::Int(n) => char::from_u32(n as u32)
                    .map(Value::Char)
                    .ok_or_else(|| Signal::Error(Diagnostic::new(Phase::Runtime, pos, "invalid char code"))),
                Value::Char(c) => Ok(Value::Char(c)),
                _ => rt(pos, "cannot cast to char"),
            },
            "str" => Ok(str_val(display(&v))),
            _ => Ok(v),
        }
    }

    // ---- builtins ----

    fn try_builtin_fn(&mut self, name: &str, args: &[Expr], env: &Env, pos: Pos) -> R<Option<Value>> {
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval(a, env)?);
        }
        let v = match name {
            "str" => Some(str_val(display(argv.get(0).unwrap_or(&Value::Unit)))),
            "len" => Some(Value::Int(self.len_of(argv.get(0).unwrap_or(&Value::Unit), pos)? as i64)),
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
            // async primitives run synchronously
            "all" => {
                let items = self.as_seq(&argv[0], pos).unwrap_or_default();
                Some(list_val(items))
            }
            "race" => Some(argv.into_iter().next().unwrap_or(Value::Nil)),
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
                    Some(if okv { ok(list_val(out)) } else { err(str_val("invalid hex")) })
                }
            }
            _ => None,
        };
        Ok(v)
    }

    fn fold_minmax(&self, args: &[Value], want_min: bool, pos: Pos) -> R<Value> {
        if args.is_empty() {
            return rt(pos, "min/max needs at least one argument");
        }
        let mut best = args[0].clone();
        for v in &args[1..] {
            let take = match (&best, v) {
                (Value::Int(a), Value::Int(b)) => if want_min { b < a } else { b > a },
                (Value::Float(a), Value::Float(b)) => if want_min { b < a } else { b > a },
                _ => return rt(pos, "min/max expects numbers of the same type"),
            };
            if take {
                best = v.clone();
            }
        }
        Ok(best)
    }

    fn call_module(&mut self, module: &str, func: &str, args: Vec<Value>, pos: Pos) -> R<Value> {
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
            ("os", "args") => Ok(list_val(self.args.iter().map(|a| str_val(a.clone())).collect())),
            ("os", "env") => {
                let key = display(&a0());
                Ok(std::env::var(&key).map(str_val).map(some).unwrap_or(Value::Nil))
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

    fn builtin_method(&mut self, recv: Value, method: &str, args: Vec<Value>, _env: &Env, pos: Pos) -> R<Value> {
        match &recv {
            // ---- universal ----
            _ if method == "len" => Ok(Value::Int(self.len_of(&recv, pos)? as i64)),
            _ => self.builtin_method_inner(recv, method, args, pos),
        }
    }

    fn builtin_method_inner(&mut self, recv: Value, method: &str, args: Vec<Value>, pos: Pos) -> R<Value> {
        match recv {
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
            Value::Enum { ref ty, ref variant, ref data } if ty.as_str() == "Option" || ty.as_str() == "Result" => {
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
                    let found = m.borrow().iter().find(|(kk, _)| value_eq(kk, &k)).map(|(_, v)| v.clone());
                    Ok(found.map(some).unwrap_or(Value::Nil))
                }
                "contains" | "contains_key" => {
                    let k = args.into_iter().next().unwrap_or(Value::Nil);
                    Ok(Value::Bool(m.borrow().iter().any(|(kk, _)| value_eq(kk, &k))))
                }
                "remove" => {
                    let k = args.into_iter().next().unwrap_or(Value::Nil);
                    m.borrow_mut().retain(|(kk, _)| !value_eq(kk, &k));
                    Ok(Value::Unit)
                }
                "keys" => Ok(list_val(m.borrow().iter().map(|(k, _)| k.clone()).collect())),
                "values" => Ok(list_val(m.borrow().iter().map(|(_, v)| v.clone()).collect())),
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

            other => rt(pos, format!("type {} has no method '{}'", other.type_name(), method)),
        }
    }

    fn str_method(&mut self, s: &str, method: &str, args: Vec<Value>, pos: Pos) -> R<Value> {
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

    fn list_method(&mut self, l: ListRef, method: &str, args: Vec<Value>, pos: Pos) -> R<Value> {
        match method {
            "push" | "append" => {
                l.borrow_mut().push(args.into_iter().next().unwrap_or(Value::Nil));
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
                        let before = self.call_value(f.clone(), vec![snapshot[j].clone(), snapshot[j - 1].clone()], pos)?;
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
                if b.iter().all(|v| matches!(v, Value::Tuple(t) if t.len() == 2)) && !b.is_empty() {
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
                Ok(if is_float { Value::Float(facc + acc as f64) } else { Value::Int(acc) })
            }
            _ => rt(pos, format!("List has no method '{}'", method)),
        }
    }
}

fn bind_params(params: &[Param], self_val: Option<Value>, args: Vec<Value>, scope: &Env) {
    let mut ai = 0;
    for p in params {
        if p.is_self {
            if let Some(sv) = self_val.clone() {
                define(scope, "self", sv, true);
            }
        } else {
            let v = args.get(ai).cloned().unwrap_or(Value::Nil);
            define(scope, &p.name, v, true);
            ai += 1;
        }
    }
}

fn make_exception(message: &str) -> Value {
    let mut m = HashMap::new();
    m.insert("message".to_string(), str_val(message.to_string()));
    Value::Struct { name: Rc::new("Exception".into()), fields: Rc::new(RefCell::new(m)) }
}

fn is_module(name: &str) -> bool {
    matches!(
        name,
        "io" | "fs" | "net" | "http" | "dns" | "tcp" | "bytes" | "crypto" | "json" | "os" | "math"
    )
}

fn module_const(module: &str, name: &str) -> Option<Value> {
    match (module, name) {
        ("math", "pi") => Some(Value::Float(std::f64::consts::PI)),
        ("math", "e") => Some(Value::Float(std::f64::consts::E)),
        ("math", "inf") => Some(Value::Float(f64::INFINITY)),
        _ => None,
    }
}

fn mask_int(n: i64, ty: &str) -> i64 {
    match ty {
        "i8" => n as i8 as i64,
        "i16" => n as i16 as i64,
        "i32" => n as i32 as i64,
        _ => n,
    }
}

fn mask_uint(n: i64, ty: &str) -> i64 {
    match ty {
        "u8" | "byte" => (n as u8) as i64,
        "u16" => (n as u16) as i64,
        "u32" => (n as u32) as i64,
        _ => n,
    }
}

fn type_matches(v: &Value, ty: &TypeExpr) -> bool {
    let name = match ty {
        TypeExpr::Named { name, .. } => name.as_str(),
        _ => return true,
    };
    match (v, name) {
        (Value::Int(_), "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" | "byte" | "int") => true,
        (Value::Float(_), "f32" | "f64" | "float") => true,
        (Value::Str(_), "str") => true,
        (Value::Bool(_), "bool") => true,
        (Value::Char(_), "char") => true,
        (Value::Struct { name: n, .. }, t) => n.as_str() == t,
        (Value::Enum { ty: n, .. }, t) => n.as_str() == t,
        _ => false,
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        // `None` and `nil` are the same value (per the spec).
        (Value::Nil, Value::Enum { variant, .. }) | (Value::Enum { variant, .. }, Value::Nil) => {
            variant.as_str() == "None"
        }
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| value_eq(p, q))
        }
        (Value::List(x), Value::List(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| value_eq(p, q))
        }
        (Value::Enum { ty: t1, variant: v1, data: d1 }, Value::Enum { ty: t2, variant: v2, data: d2 }) => {
            t1 == t2 && v1 == v2 && d1.len() == d2.len() && d1.iter().zip(d2.iter()).all(|(p, q)| value_eq(p, q))
        }
        (Value::Struct { name: n1, fields: f1 }, Value::Struct { name: n2, fields: f2 }) => {
            n1 == n2 && {
                let (f1, f2) = (f1.borrow(), f2.borrow());
                f1.len() == f2.len() && f1.iter().all(|(k, v)| f2.get(k).map_or(false, |w| value_eq(v, w)))
            }
        }
        (Value::Unit, Value::Unit) => true,
        _ => false,
    }
}

/// Human-facing rendering used by `io.println`, `str()`, and f-strings.
pub fn display(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{:.1}", f)
            } else {
                format!("{}", f)
            }
        }
        Value::Char(c) => c.to_string(),
        Value::Str(s) => s.to_string(),
        Value::Unit => "()".into(),
        Value::List(l) => {
            let inner: Vec<String> = l.borrow().iter().map(display).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Set(s) => {
            let inner: Vec<String> = s.borrow().iter().map(display).collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Map(m) => {
            let inner: Vec<String> = m.borrow().iter().map(|(k, v)| format!("{}: {}", display(k), display(v))).collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Tuple(t) => {
            let inner: Vec<String> = t.iter().map(display).collect();
            format!("({})", inner.join(", "))
        }
        Value::Range { start, end, inclusive } => {
            if *inclusive {
                format!("{}..={}", start, end)
            } else {
                format!("{}..{}", start, end)
            }
        }
        Value::Struct { name, fields } => {
            let f = fields.borrow();
            let inner: Vec<String> = f.iter().map(|(k, v)| format!("{}: {}", k, display(v))).collect();
            format!("{} {{ {} }}", name, inner.join(", "))
        }
        Value::Enum { variant, data, .. } => {
            if data.is_empty() {
                variant.to_string()
            } else {
                let inner: Vec<String> = data.iter().map(display).collect();
                format!("{}({})", variant, inner.join(", "))
            }
        }
        Value::Closure(_) | Value::Function(_) => "<fn>".into(),
    }
}

fn json_encode(v: &Value, pretty: bool, indent: usize) -> String {
    let pad = if pretty { "  ".repeat(indent + 1) } else { String::new() };
    let pad_end = if pretty { "  ".repeat(indent) } else { String::new() };
    let nl = if pretty { "\n" } else { "" };
    let sep = if pretty { ",\n" } else { "," };
    match v {
        Value::Nil => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format!("{}", f),
        Value::Str(s) => json_string(s),
        Value::Char(c) => json_string(&c.to_string()),
        Value::List(l) => {
            let items: Vec<String> = l
                .borrow()
                .iter()
                .map(|x| format!("{}{}", pad, json_encode(x, pretty, indent + 1)))
                .collect();
            if items.is_empty() {
                "[]".into()
            } else {
                format!("[{}{}{}{}]", nl, items.join(sep), nl, pad_end)
            }
        }
        Value::Map(m) => {
            let items: Vec<String> = m
                .borrow()
                .iter()
                .map(|(k, val)| {
                    format!("{}{}: {}", pad, json_string(&display(k)), json_encode(val, pretty, indent + 1))
                })
                .collect();
            if items.is_empty() {
                "{}".into()
            } else {
                format!("{{{}{}{}{}}}", nl, items.join(sep), nl, pad_end)
            }
        }
        Value::Struct { fields, .. } => {
            let items: Vec<String> = fields
                .borrow()
                .iter()
                .map(|(k, val)| format!("{}{}: {}", pad, json_string(k), json_encode(val, pretty, indent + 1)))
                .collect();
            format!("{{{}{}{}{}}}", nl, items.join(sep), nl, pad_end)
        }
        _ => json_string(&display(v)),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A small recursive-descent JSON parser producing La3 values.
fn json_decode(s: &str) -> std::result::Result<Value, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let v = json_value(&chars, &mut i)?;
    json_ws(&chars, &mut i);
    if i != chars.len() {
        return Err("trailing characters after JSON value".into());
    }
    Ok(v)
}

fn json_ws(c: &[char], i: &mut usize) {
    while *i < c.len() && c[*i].is_whitespace() {
        *i += 1;
    }
}

fn json_value(c: &[char], i: &mut usize) -> std::result::Result<Value, String> {
    json_ws(c, i);
    if *i >= c.len() {
        return Err("unexpected end of JSON".into());
    }
    match c[*i] {
        '{' => {
            *i += 1;
            let mut entries = Vec::new();
            json_ws(c, i);
            if c.get(*i) == Some(&'}') {
                *i += 1;
                return Ok(Value::Map(Rc::new(RefCell::new(entries))));
            }
            loop {
                json_ws(c, i);
                let key = json_str(c, i)?;
                json_ws(c, i);
                if c.get(*i) != Some(&':') {
                    return Err("expected ':' in object".into());
                }
                *i += 1;
                let val = json_value(c, i)?;
                entries.push((str_val(key), val));
                json_ws(c, i);
                match c.get(*i) {
                    Some(',') => {
                        *i += 1;
                    }
                    Some('}') => {
                        *i += 1;
                        break;
                    }
                    _ => return Err("expected ',' or '}' in object".into()),
                }
            }
            Ok(Value::Map(Rc::new(RefCell::new(entries))))
        }
        '[' => {
            *i += 1;
            let mut items = Vec::new();
            json_ws(c, i);
            if c.get(*i) == Some(&']') {
                *i += 1;
                return Ok(list_val(items));
            }
            loop {
                let v = json_value(c, i)?;
                items.push(v);
                json_ws(c, i);
                match c.get(*i) {
                    Some(',') => {
                        *i += 1;
                    }
                    Some(']') => {
                        *i += 1;
                        break;
                    }
                    _ => return Err("expected ',' or ']' in array".into()),
                }
            }
            Ok(list_val(items))
        }
        '"' => Ok(str_val(json_str(c, i)?)),
        't' => {
            json_lit(c, i, "true")?;
            Ok(Value::Bool(true))
        }
        'f' => {
            json_lit(c, i, "false")?;
            Ok(Value::Bool(false))
        }
        'n' => {
            json_lit(c, i, "null")?;
            Ok(Value::Nil)
        }
        _ => json_number(c, i),
    }
}

fn json_str(c: &[char], i: &mut usize) -> std::result::Result<String, String> {
    if c.get(*i) != Some(&'"') {
        return Err("expected string".into());
    }
    *i += 1;
    let mut out = String::new();
    while let Some(&ch) = c.get(*i) {
        *i += 1;
        match ch {
            '"' => return Ok(out),
            '\\' => {
                let e = c.get(*i).copied().ok_or("bad escape")?;
                *i += 1;
                out.push(match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    other => other,
                });
            }
            _ => out.push(ch),
        }
    }
    Err("unterminated string".into())
}

fn json_number(c: &[char], i: &mut usize) -> std::result::Result<Value, String> {
    let start = *i;
    let mut is_float = false;
    while let Some(&ch) = c.get(*i) {
        if ch.is_ascii_digit() || ch == '-' || ch == '+' {
            *i += 1;
        } else if ch == '.' || ch == 'e' || ch == 'E' {
            is_float = true;
            *i += 1;
        } else {
            break;
        }
    }
    let text: String = c[start..*i].iter().collect();
    if text.is_empty() {
        return Err("invalid JSON value".into());
    }
    if is_float {
        text.parse::<f64>().map(Value::Float).map_err(|_| "invalid number".into())
    } else {
        text.parse::<i64>().map(Value::Int).map_err(|_| "invalid number".into())
    }
}

fn json_lit(c: &[char], i: &mut usize, lit: &str) -> std::result::Result<(), String> {
    for expect in lit.chars() {
        if c.get(*i) != Some(&expect) {
            return Err(format!("expected '{}'", lit));
        }
        *i += 1;
    }
    Ok(())
}

fn format_value(v: &Value, spec: Option<&str>, _pos: Pos) -> R<String> {
    let spec = match spec {
        None => return Ok(display(v)),
        Some(s) => s.trim(),
    };
    // hex like `02x` / `x`
    if spec.ends_with('x') || spec.ends_with('X') {
        if let Value::Int(n) = v {
            let upper = spec.ends_with('X');
            let width: usize = spec[..spec.len() - 1].trim_start_matches('0').parse().unwrap_or(0);
            let zero = spec[..spec.len() - 1].starts_with('0');
            let body = if upper { format!("{:X}", n) } else { format!("{:x}", n) };
            if zero && body.len() < width {
                return Ok(format!("{}{}", "0".repeat(width - body.len()), body));
            }
            return Ok(body);
        }
    }
    // float precision like `.1f`
    if let Some(rest) = spec.strip_prefix('.') {
        if let Some(prec) = rest.strip_suffix('f') {
            if let Ok(p) = prec.parse::<usize>() {
                let f = match v {
                    Value::Float(f) => *f,
                    Value::Int(n) => *n as f64,
                    _ => return Ok(display(v)),
                };
                return Ok(format!("{:.*}", p, f));
            }
        }
    }
    // alignment like `>20` / `<20`
    if let Some(rest) = spec.strip_prefix('>') {
        if let Ok(w) = rest.parse::<usize>() {
            return Ok(format!("{:>width$}", display(v), width = w));
        }
    }
    if let Some(rest) = spec.strip_prefix('<') {
        if let Ok(w) = rest.parse::<usize>() {
            return Ok(format!("{:<width$}", display(v), width = w));
        }
    }
    Ok(display(v))
}

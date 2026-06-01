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
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos, Result as DResult};

mod builtins;
mod calls;
mod concurrency;
mod convert;
mod exprs;
mod loops;
mod matching;
mod stmts;

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
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
    },
    Struct {
        name: Rc<String>,
        fields: FieldsRef,
    },
    Enum {
        ty: Rc<String>,
        variant: Rc<String>,
        data: Rc<Vec<Value>>,
    },
    Closure(Rc<ClosureData>),
    Function(Rc<FnDecl>),
    /// A typed communication channel (Section 12). Shared by reference so the
    /// same channel can be sent on by one task and received from by another.
    Channel(Rc<RefCell<ChannelData>>),
    /// A spawned task or future. It runs cooperatively to completion the first
    /// time its result is needed (`join`, `await`, a blocked `recv`, or program
    /// shutdown); the result is then memoized.
    Future(Rc<TaskState>),
    /// A safe reference (`&T` / `&mut T`): an alias to a variable's storage cell.
    /// Reads and writes through it (`*r`, `*r = v`) reach the original binding.
    Ref(Rc<RefCell<Value>>, bool),
    /// A raw pointer (`*T` / `*mut T`): an element-addressed cursor into a
    /// backing store, either an array/list or a heap region from `alloc`.
    /// Arithmetic is scaled by element, so `*(p + n)` is the n-th element.
    Ptr(Rc<PtrData>),
    Unit,
}

/// A raw pointer. Indexing is element-based, so `p + n` selects the element `n`
/// steps along; `elem_size` records `sizeof(T)` for the byte-offset view the
/// reference describes (`p + n` advances `n * sizeof(T)` bytes).
pub struct PtrData {
    store: Rc<RefCell<Vec<Value>>>,
    index: i64,
    elem_size: usize,
    mutable: bool,
}

pub struct ClosureData {
    params: Vec<Param>,
    body: Expr,
    env: Env,
}

/// A buffered channel. The `capacity` is advisory in v0.1; the cooperative
/// scheduler runs producer tasks to completion, so the buffer is never bounded
/// in a way that could deadlock a single-threaded run.
pub struct ChannelData {
    buf: VecDeque<Value>,
    closed: bool,
    capacity: Option<usize>,
}

/// The body of a spawned task plus its memoized result once it has run.
pub struct TaskState {
    body: Block,
    env: Env,
    result: RefCell<Option<Value>>,
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
            Value::Channel(_) => "Channel".into(),
            Value::Future(_) => "Future".into(),
            Value::Ref(_, _) => "ref".into(),
            Value::Ptr(_) => "ptr".into(),
            Value::Unit => "()".into(),
        }
    }
}

fn some(v: Value) -> Value {
    Value::Enum {
        ty: Rc::new("Option".into()),
        variant: Rc::new("Some".into()),
        data: Rc::new(vec![v]),
    }
}
fn ok(v: Value) -> Value {
    Value::Enum {
        ty: Rc::new("Result".into()),
        variant: Rc::new("Ok".into()),
        data: Rc::new(vec![v]),
    }
}
fn err(v: Value) -> Value {
    Value::Enum {
        ty: Rc::new("Result".into()),
        variant: Rc::new("Err".into()),
        data: Rc::new(vec![v]),
    }
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
    Rc::new(RefCell::new(Scope {
        vars: HashMap::new(),
        parent,
    }))
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
        Var {
            cell: Rc::new(RefCell::new(value)),
            mutable,
        },
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
    /// Spawned tasks that have not yet been forced to completion. The scheduler
    /// drains this queue when a `recv` blocks and again at program shutdown.
    ready: VecDeque<Rc<TaskState>>,
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
            ready: VecDeque::new(),
        }
    }

    /// Set the arguments returned by `os.args()`.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.args = args;
    }

    /// Load all items, then call `main` if present.
    pub fn run(&mut self, prog: &Program) -> DResult<()> {
        self.load(prog);
        let result = if lookup(&self.globals, "main").is_some() {
            let main = lookup(&self.globals, "main").unwrap();
            let f = main.borrow().clone();
            match self.call_value(f, vec![], Pos::default()) {
                Ok(_) => Ok(()),
                Err(Signal::Error(d)) => Err(d),
                Err(_) => Ok(()),
            }
        } else {
            Ok(())
        };
        // Run any spawned tasks that were never joined, so fire-and-forget side
        // effects (e.g. a `spawn` that only prints) still happen.
        while let Ok(true) = self.run_one_ready(Pos::default()) {}
        result
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
                    define(
                        &self.globals,
                        &f.name,
                        Value::Function(Rc::new(f.clone())),
                        false,
                    );
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
    Value::Struct {
        name: Rc::new("Exception".into()),
        fields: Rc::new(RefCell::new(m)),
    }
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
        (
            Value::Int(_),
            "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize"
            | "byte" | "int",
        ) => true,
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
        (
            Value::Enum {
                ty: t1,
                variant: v1,
                data: d1,
            },
            Value::Enum {
                ty: t2,
                variant: v2,
                data: d2,
            },
        ) => {
            t1 == t2
                && v1 == v2
                && d1.len() == d2.len()
                && d1.iter().zip(d2.iter()).all(|(p, q)| value_eq(p, q))
        }
        (
            Value::Struct {
                name: n1,
                fields: f1,
            },
            Value::Struct {
                name: n2,
                fields: f2,
            },
        ) => {
            n1 == n2 && {
                let (f1, f2) = (f1.borrow(), f2.borrow());
                f1.len() == f2.len()
                    && f1
                        .iter()
                        .all(|(k, v)| f2.get(k).map_or(false, |w| value_eq(v, w)))
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
            let inner: Vec<String> = m
                .borrow()
                .iter()
                .map(|(k, v)| format!("{}: {}", display(k), display(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Tuple(t) => {
            let inner: Vec<String> = t.iter().map(display).collect();
            format!("({})", inner.join(", "))
        }
        Value::Range {
            start,
            end,
            inclusive,
        } => {
            if *inclusive {
                format!("{}..={}", start, end)
            } else {
                format!("{}..{}", start, end)
            }
        }
        Value::Struct { name, fields } => {
            let f = fields.borrow();
            let inner: Vec<String> = f
                .iter()
                .map(|(k, v)| format!("{}: {}", k, display(v)))
                .collect();
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
        Value::Channel(_) => "<channel>".into(),
        Value::Future(_) => "<future>".into(),
        // A reference prints as the value it points to (auto-deref).
        Value::Ref(cell, _) => display(&cell.borrow()),
        Value::Ptr(_) => "<ptr>".into(),
    }
}

/// A best-effort `sizeof(T)` for the byte-offset view of pointer arithmetic.
/// Indexing itself is element-based, so this only affects the reported scale.
fn size_of_value(v: &Value) -> usize {
    match v {
        Value::Bool(_) => 1,
        Value::Char(_) => 4,
        Value::Float(_) => 8,
        Value::Int(_) => 8,
        _ => 1,
    }
}

fn json_encode(v: &Value, pretty: bool, indent: usize) -> String {
    let pad = if pretty {
        "  ".repeat(indent + 1)
    } else {
        String::new()
    };
    let pad_end = if pretty {
        "  ".repeat(indent)
    } else {
        String::new()
    };
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
                    format!(
                        "{}{}: {}",
                        pad,
                        json_string(&display(k)),
                        json_encode(val, pretty, indent + 1)
                    )
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
                .map(|(k, val)| {
                    format!(
                        "{}{}: {}",
                        pad,
                        json_string(k),
                        json_encode(val, pretty, indent + 1)
                    )
                })
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
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| "invalid number".into())
    } else {
        text.parse::<i64>()
            .map(Value::Int)
            .map_err(|_| "invalid number".into())
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
            let width: usize = spec[..spec.len() - 1]
                .trim_start_matches('0')
                .parse()
                .unwrap_or(0);
            let zero = spec[..spec.len() - 1].starts_with('0');
            let body = if upper {
                format!("{:X}", n)
            } else {
                format!("{:x}", n)
            };
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

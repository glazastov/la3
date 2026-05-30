//! Abstract syntax tree for La3.
//!
//! Types are parsed into [`TypeExpr`] but the v0.1 checker uses them only for
//! light validation; the interpreter is value-driven. Spans are attached to
//! expressions and statements so diagnostics can point at source.
//!
//! The AST is a faithful, complete record of the parsed syntax: every field is
//! populated by the parser and printed by `la3 ast` through the derived `Debug`.
//! The v0.1 interpreter and checker read a subset of these fields, and dead-code
//! analysis does not count `Debug` as a use, so this module opts out of the
//! `dead_code` lint rather than dropping syntax the AST is meant to preserve.
#![allow(dead_code)]

use crate::diag::Pos;

#[derive(Clone, Debug)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Impl(ImplBlock),
    Const(ConstDecl),
    /// Parsed and ignored at runtime in v0.1 (kept so real La3 files load).
    Use(Vec<String>),
    TypeAlias { name: String, ty: TypeExpr },
    Interface(InterfaceDecl),
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: String,
    pub generics: Vec<String>,
    pub params: Vec<Param>,
    pub variadic: Option<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub is_async: bool,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    /// `self`, `mut self`, `&self`, `&mut self`
    pub is_self: bool,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: String,
    pub generics: Vec<String>,
    pub fields: Vec<(String, TypeExpr)>,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: String,
    pub generics: Vec<String>,
    pub variants: Vec<EnumVariant>,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct EnumVariant {
    pub name: String,
    pub kind: VariantKind,
}

#[derive(Clone, Debug)]
pub enum VariantKind {
    Unit,
    Tuple(usize),
    Struct(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct ImplBlock {
    /// `impl Type` or `impl Interface for Type`
    pub interface: Option<String>,
    pub ty: String,
    pub methods: Vec<FnDecl>,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct InterfaceDecl {
    pub name: String,
    pub supers: Vec<String>,
    pub methods: Vec<String>,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct ConstDecl {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The trailing expression, if the block ends with one (its value).
    pub tail: Option<Box<Expr>>,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let {
        pattern: Pattern,
        mutable: bool,
        ty: Option<TypeExpr>,
        value: Expr,
        pos: Pos,
    },
    Expr(Expr),
    Return(Option<Expr>, Pos),
    Break(Option<Expr>, Pos),
    Continue(Pos),
    /// Local item (e.g. a nested const); rare but allowed.
    Item(Item),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub pos: Pos,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    /// Segments of an f-string: literal text or an embedded expression with an
    /// optional format spec (the text after `:`).
    FStr(Vec<FStrPart>),
    Char(char),
    Bool(bool),
    Nil,
    Ident(String),
    SelfExpr,

    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `a ?? b`
    Coalesce {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        op: Option<BinOp>, // None = `=`, Some = compound like `+=`
        value: Box<Expr>,
    },
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        recv: Box<Expr>,
        /// `?.` short-circuits on nil.
        optional: bool,
        method: String,
        /// Turbofish type args, parsed and ignored at runtime.
        type_args: Vec<TypeExpr>,
        args: Vec<Expr>,
    },
    Field {
        recv: Box<Expr>,
        optional: bool,
        name: String,
    },
    Index {
        recv: Box<Expr>,
        index: Box<Expr>,
    },
    /// `Type::method` or `Module::item` path used as a value or callee.
    Path(Vec<String>),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    /// `[value; count]`
    ListRepeat {
        value: Box<Expr>,
        count: Box<Expr>,
    },
    Map(Vec<(Expr, Expr)>),
    Set(Vec<Expr>),
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
        /// `..other`
        spread: Option<Box<Expr>>,
    },
    Block(Block),
    If {
        cond: Box<Expr>,
        then: Block,
        els: Option<Box<Expr>>, // Block or another If
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Loop {
        body: Block,
    },
    While {
        cond: Box<Expr>,
        body: Block,
    },
    /// `while let PAT = EXPR { ... }`
    WhileLet {
        pattern: Pattern,
        expr: Box<Expr>,
        body: Block,
    },
    For {
        pattern: Pattern,
        iter: Box<Expr>,
        body: Block,
    },
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    Closure {
        params: Vec<Param>,
        body: Box<Expr>,
        is_move: bool,
    },
    /// `expr?` error/none propagation.
    Try(Box<Expr>),
    Await(Box<Expr>),
    Spawn(Block),
    Unsafe(Block),
    TryCatch {
        body: Block,
        catches: Vec<CatchArm>,
        finally: Option<Block>,
    },
}

#[derive(Clone, Debug)]
pub enum FStrPart {
    Lit(String),
    Expr { expr: Box<Expr>, spec: Option<String> },
}

#[derive(Clone, Debug)]
pub struct CatchArm {
    pub binding: Option<String>,
    pub ty: Option<String>,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Wildcard,
    Int(i64),
    Str(String),
    Bool(bool),
    Char(char),
    Nil,
    /// A plain binding name.
    Binding(String),
    /// `name @ subpattern`
    At(String, Box<Pattern>),
    /// `lo..=hi` or `lo..hi`
    Range { lo: i64, hi: i64, inclusive: bool },
    Tuple(Vec<Pattern>),
    /// `[a, b, ..rest]`
    List { items: Vec<Pattern>, rest: Option<String> },
    /// `Enum.Variant(p, ..)` or `Variant(p, ..)` or bare `Variant`
    Variant { path: Vec<String>, args: Vec<Pattern> },
    /// `Type { a, b }`
    Struct { name: String, fields: Vec<String> },
    /// `name: Type` type-narrowing pattern in a union match.
    Typed { binding: String, ty: TypeExpr },
    /// `a | b | c`
    Or(Vec<Pattern>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
    Deref,
    Ref,
    RefMut,
    RawRef,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A type as written in source. Used lightly in v0.1.
#[derive(Clone, Debug)]
pub enum TypeExpr {
    Named { name: String, args: Vec<TypeExpr> },
    Ref { mutable: bool, inner: Box<TypeExpr> },
    Ptr { mutable: bool, inner: Box<TypeExpr> },
    Array { inner: Box<TypeExpr>, size: Option<i64> },
    Slice(Box<TypeExpr>),
    Tuple(Vec<TypeExpr>),
    Union(Vec<TypeExpr>),
    Fn { params: Vec<TypeExpr>, ret: Box<TypeExpr> },
    Async(Box<TypeExpr>),
    Unit,
    Never,
}

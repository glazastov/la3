//! Recursive-descent parser with a Pratt expression core.
//!
//! Newlines: La3 has optional semicolons. Before parsing we drop newlines that
//! cannot terminate a statement (those next to an operator, an open delimiter,
//! a comma, a continuation keyword, and so on). The newlines that survive act
//! as statement terminators inside blocks.

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos, Result};
use crate::lexer::{Lexer, Tok, Token};

pub fn parse(src: &str) -> Result<Program> {
    let tokens = Lexer::new(src).tokenize()?;
    let tokens = filter_newlines(tokens);
    let mut prog = Parser::new(tokens).parse_program()?;
    // Number every expression so the type checker and later passes can key
    // side tables on a unique NodeId (see ast::Program::assign_ids).
    prog.assign_ids();
    Ok(prog)
}

/// Parse a single expression (used by the f-string sub-parser).
fn parse_expr_str(src: &str, pos: Pos) -> Result<Expr> {
    let tokens = Lexer::new(src).tokenize().map_err(|mut d| {
        d.pos = pos;
        d
    })?;
    let tokens = filter_newlines(tokens);
    let mut p = Parser::new(tokens);
    let e = p.parse_expr()?;
    Ok(e)
}

/// The interface name of a generic bound, e.g. `Encode` from `T: Encode`.
/// Bounds are written as types; only the head name is kept for conformance.
fn bound_name(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named { name, .. } => name.clone(),
        _ => String::new(),
    }
}

fn ends_expr(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Int(_)
            | Tok::Float(_)
            | Tok::Str(_)
            | Tok::FStr(_)
            | Tok::Char(_)
            | Tok::Ident(_)
            | Tok::SelfKw
            | Tok::True
            | Tok::False
            | Tok::Nil
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Question
            | Tok::Break
            | Tok::Continue
            | Tok::Return
    )
}

/// A newline before one of these tokens is never a terminator (the statement
/// continues onto the next line).
fn continues_after(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Dot
            | Tok::QuestionDot
            | Tok::Plus
            | Tok::Minus
            // `*` and `&` are intentionally absent: a line starting with one is a
            // dereference/reference statement (`*p = v`), not a continued binary
            // multiply/bit-and. Continuation puts the operator at the line end
            // (see `open_before`), so this only affects leading `*` / `&`.
            | Tok::StarStar
            | Tok::Slash
            | Tok::Percent
            | Tok::AmpAmp
            | Tok::PipePipe
            | Tok::Pipe
            | Tok::Caret
            | Tok::Shl
            | Tok::Shr
            | Tok::EqEq
            | Tok::Ne
            | Tok::Lt
            | Tok::Gt
            | Tok::Le
            | Tok::Ge
            | Tok::DotDot
            | Tok::DotDotEq
            | Tok::QuestionQuestion
            | Tok::FatArrow
            | Tok::Arrow
            | Tok::Comma
            | Tok::Colon
            | Tok::ColonColon
            | Tok::As
            | Tok::Else
            | Tok::RParen
            | Tok::RBracket
            | Tok::Eq
            | Tok::PlusEq
            | Tok::MinusEq
            | Tok::StarEq
            | Tok::SlashEq
            | Tok::PercentEq
    )
}

/// A newline after one of these tokens never terminates (the line is mid-expr).
fn open_before(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Plus
            | Tok::Minus
            | Tok::Star
            | Tok::StarStar
            | Tok::Slash
            | Tok::Percent
            | Tok::Amp
            | Tok::AmpAmp
            | Tok::Pipe
            | Tok::PipePipe
            | Tok::Caret
            | Tok::Tilde
            | Tok::Shl
            | Tok::Shr
            | Tok::Eq
            | Tok::EqEq
            | Tok::Ne
            | Tok::Lt
            | Tok::Gt
            | Tok::Le
            | Tok::Ge
            | Tok::PlusEq
            | Tok::MinusEq
            | Tok::StarEq
            | Tok::SlashEq
            | Tok::PercentEq
            | Tok::DotDot
            | Tok::DotDotEq
            | Tok::QuestionQuestion
            | Tok::QuestionDot
            | Tok::Arrow
            | Tok::FatArrow
            | Tok::Dot
            | Tok::Comma
            | Tok::Colon
            | Tok::ColonColon
            | Tok::At
            | Tok::LParen
            | Tok::LBrace
            | Tok::LBracket
            | Tok::As
            | Tok::Let
            | Tok::Return
            | Tok::In
            | Tok::Match
            | Tok::Else
            | Tok::Bang
    )
}

fn filter_newlines(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for (idx, t) in tokens.iter().enumerate() {
        if matches!(t.tok, Tok::Newline) {
            let prev = out.last().map(|p| &p.tok);
            let next = tokens.get(idx + 1).map(|n| &n.tok);
            // Drop a newline if the previous token leaves an expression open.
            if let Some(p) = prev {
                if open_before(p) {
                    continue;
                }
            } else {
                continue; // leading newline
            }
            // Drop a newline if the next token continues the statement.
            if let Some(n) = next {
                if continues_after(n) || matches!(n, Tok::RBrace) {
                    continue;
                }
            }
            // Collapse runs of newlines.
            if matches!(prev, Some(Tok::Newline)) {
                continue;
            }
            // Keep a newline only after something that can end an expression
            // or a closing brace / item keyword boundary.
            let keep = prev.map_or(false, |p| {
                ends_expr(p) || matches!(p, Tok::RBrace | Tok::Semicolon)
            });
            if !keep {
                continue;
            }
        }
        out.push(t.clone());
    }
    out
}

struct Parser {
    toks: Vec<Token>,
    i: usize,
    /// When true, `Ident {` is not parsed as a struct literal (used in the
    /// condition position of if/while/for/match).
    no_struct: bool,
}

impl Parser {
    fn new(toks: Vec<Token>) -> Self {
        Parser {
            toks,
            i: 0,
            no_struct: false,
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        self.toks
            .get(self.i + n)
            .map(|t| &t.tok)
            .unwrap_or(&Tok::Eof)
    }

    fn pos(&self) -> Pos {
        self.toks[self.i].pos
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(Diagnostic::new(Phase::Parse, self.pos(), msg))
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<()> {
        if self.at(t) {
            self.bump();
            Ok(())
        } else {
            self.err(format!("expected {}, found {:?}", what, self.peek()))
        }
    }

    fn skip_terminators(&mut self) {
        while matches!(self.peek(), Tok::Newline | Tok::Semicolon) {
            self.bump();
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            other => self.err(format!("expected identifier, found {:?}", other)),
        }
    }

    // ---- items ----

    fn parse_program(&mut self) -> Result<Program> {
        let mut items = Vec::new();
        self.skip_terminators();
        while !matches!(self.peek(), Tok::Eof) {
            self.parse_item_into(&mut items)?;
            self.skip_terminators();
        }
        Ok(Program { items })
    }

    fn parse_item_into(&mut self, items: &mut Vec<Item>) -> Result<()> {
        // `pub` is parsed and ignored for visibility in v0.1.
        self.eat(&Tok::Pub);
        match self.peek() {
            Tok::Fn | Tok::Async => items.push(Item::Fn(self.parse_fn()?)),
            Tok::Struct => items.push(Item::Struct(self.parse_struct()?)),
            Tok::Enum => items.push(Item::Enum(self.parse_enum()?)),
            Tok::Impl => items.push(Item::Impl(self.parse_impl()?)),
            Tok::Const => items.push(Item::Const(self.parse_const()?)),
            Tok::Interface => items.push(Item::Interface(self.parse_interface()?)),
            Tok::Type => items.push(self.parse_type_alias()?),
            Tok::Use => items.push(self.parse_use()?),
            Tok::Mod => {
                // Parse and hoist module items (namespacing is ignored in v0.1).
                self.bump();
                let _name = self.ident()?;
                self.expect(&Tok::LBrace, "'{'")?;
                self.skip_terminators();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    self.parse_item_into(items)?;
                    self.skip_terminators();
                }
                self.expect(&Tok::RBrace, "'}'")?;
            }
            other => return self.err(format!("expected item, found {:?}", other)),
        }
        Ok(())
    }

    fn parse_use(&mut self) -> Result<Item> {
        self.bump();
        let mut path = Vec::new();
        loop {
            if self.at(&Tok::LBrace) {
                self.bump();
                while !self.at(&Tok::RBrace) {
                    path.push(self.ident()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBrace, "'}'")?;
                break;
            }
            path.push(self.ident()?);
            if !self.eat(&Tok::ColonColon) {
                break;
            }
        }
        Ok(Item::Use(path))
    }

    fn parse_type_alias(&mut self) -> Result<Item> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::Eq, "'='")?;
        let ty = self.parse_type()?;
        Ok(Item::TypeAlias { name, ty })
    }

    /// Parse `<T, U: Bound + Bound>`, keeping each parameter's interface bounds.
    fn parse_generics(&mut self) -> Result<Vec<(String, Vec<String>)>> {
        let mut g = Vec::new();
        if self.eat(&Tok::Lt) {
            while !self.at(&Tok::Gt) {
                let name = self.ident()?;
                let mut bounds = Vec::new();
                // optional bound `: Bound + Bound`
                if self.eat(&Tok::Colon) {
                    bounds.push(bound_name(&self.parse_type()?));
                    while self.eat(&Tok::Plus) {
                        bounds.push(bound_name(&self.parse_type()?));
                    }
                }
                g.push((name, bounds));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::Gt, "'>'")?;
        }
        Ok(g)
    }

    /// The plain generic-parameter names, dropping bounds. Used where only the
    /// names matter (struct and enum declarations).
    fn parse_generic_names(&mut self) -> Result<Vec<String>> {
        Ok(self.parse_generics()?.into_iter().map(|(n, _)| n).collect())
    }

    fn parse_fn(&mut self) -> Result<FnDecl> {
        let pos = self.pos();
        let is_async = self.eat(&Tok::Async);
        self.expect(&Tok::Fn, "'fn'")?;
        let name = self.ident()?;
        let generics = self.parse_generics()?;
        self.expect(&Tok::LParen, "'('")?;
        let mut params = Vec::new();
        let mut variadic = None;
        while !self.at(&Tok::RParen) {
            if self.eat(&Tok::DotDot) {
                // `...name: T` variadic (spelled `..` here after newline filter)
            }
            // self receivers
            if self.at(&Tok::Amp) {
                self.bump();
                let _m = self.eat(&Tok::Mut);
                if self.eat(&Tok::SelfKw) {
                    params.push(Param {
                        name: "self".into(),
                        ty: None,
                        is_self: true,
                    });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                    continue;
                } else {
                    return self.err("expected 'self' after '&'");
                }
            }
            if self.at(&Tok::Mut) && matches!(self.peek_at(1), Tok::SelfKw) {
                self.bump();
                self.bump();
                params.push(Param {
                    name: "self".into(),
                    ty: None,
                    is_self: true,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
                continue;
            }
            if self.eat(&Tok::SelfKw) {
                params.push(Param {
                    name: "self".into(),
                    ty: None,
                    is_self: true,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
                continue;
            }
            // variadic `...args: T`
            let is_variadic = self.eat(&Tok::DotDotEq) || self.eat(&Tok::DotDot);
            let name = self.ident()?;
            let ty = if self.eat(&Tok::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let p = Param {
                name,
                ty,
                is_self: false,
            };
            if is_variadic {
                variadic = Some(p);
            } else {
                params.push(p);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(FnDecl {
            name,
            generics,
            params,
            variadic,
            ret,
            body,
            is_async,
            pos,
        })
    }

    fn parse_struct(&mut self) -> Result<StructDecl> {
        let pos = self.pos();
        self.bump();
        let name = self.ident()?;
        let generics = self.parse_generic_names()?;
        let mut fields = Vec::new();
        self.expect(&Tok::LBrace, "'{'")?;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) {
            let fname = self.ident()?;
            self.expect(&Tok::Colon, "':'")?;
            let ty = self.parse_type()?;
            fields.push((fname, ty));
            self.eat(&Tok::Comma);
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(StructDecl {
            name,
            generics,
            fields,
            pos,
        })
    }

    fn parse_enum(&mut self) -> Result<EnumDecl> {
        let pos = self.pos();
        self.bump();
        let name = self.ident()?;
        let generics = self.parse_generic_names()?;
        let mut variants = Vec::new();
        self.expect(&Tok::LBrace, "'{'")?;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) {
            let vname = self.ident()?;
            let kind = if self.eat(&Tok::LParen) {
                let mut n = 0;
                while !self.at(&Tok::RParen) {
                    self.parse_type()?;
                    n += 1;
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "')'")?;
                VariantKind::Tuple(n)
            } else if self.eat(&Tok::LBrace) {
                let mut names = Vec::new();
                while !self.at(&Tok::RBrace) {
                    names.push(self.ident()?);
                    self.expect(&Tok::Colon, "':'")?;
                    self.parse_type()?;
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBrace, "'}'")?;
                VariantKind::Struct(names)
            } else {
                VariantKind::Unit
            };
            variants.push(EnumVariant { name: vname, kind });
            self.eat(&Tok::Comma);
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(EnumDecl {
            name,
            generics,
            variants,
            pos,
        })
    }

    fn parse_impl(&mut self) -> Result<ImplBlock> {
        let pos = self.pos();
        self.bump();
        let first = self.ident()?;
        let (interface, ty) = if self.eat(&Tok::For) {
            (Some(first), self.ident()?)
        } else {
            (None, first)
        };
        // ignore generic args on the type
        if self.eat(&Tok::Lt) {
            let mut depth = 1;
            while depth > 0 && !matches!(self.peek(), Tok::Eof) {
                match self.bump().tok {
                    Tok::Lt => depth += 1,
                    Tok::Gt => depth -= 1,
                    _ => {}
                }
            }
        }
        let mut methods = Vec::new();
        self.expect(&Tok::LBrace, "'{'")?;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) {
            self.eat(&Tok::Pub);
            methods.push(self.parse_fn()?);
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(ImplBlock {
            interface,
            ty,
            methods,
            pos,
        })
    }

    fn parse_interface(&mut self) -> Result<InterfaceDecl> {
        let pos = self.pos();
        self.bump();
        let name = self.ident()?;
        let mut supers = Vec::new();
        if self.eat(&Tok::Colon) {
            supers.push(self.ident()?);
            while self.eat(&Tok::Plus) {
                supers.push(self.ident()?);
            }
        }
        let mut methods = Vec::new();
        self.expect(&Tok::LBrace, "'{'")?;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) {
            // method signatures; parse `fn name(...) -> T` and discard the shape
            self.expect(&Tok::Fn, "'fn'")?;
            methods.push(self.ident()?);
            self.expect(&Tok::LParen, "'('")?;
            let mut depth = 1;
            while depth > 0 && !matches!(self.peek(), Tok::Eof) {
                match self.bump().tok {
                    Tok::LParen => depth += 1,
                    Tok::RParen => depth -= 1,
                    _ => {}
                }
            }
            if self.eat(&Tok::Arrow) {
                self.parse_type()?;
            }
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(InterfaceDecl {
            name,
            supers,
            methods,
            pos,
        })
    }

    fn parse_const(&mut self) -> Result<ConstDecl> {
        let pos = self.pos();
        self.bump();
        let name = self.ident()?;
        let ty = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Tok::Eq, "'='")?;
        let value = self.parse_expr()?;
        Ok(ConstDecl {
            name,
            ty,
            value,
            pos,
        })
    }

    // ---- types ----

    fn parse_type(&mut self) -> Result<TypeExpr> {
        let mut t = self.parse_type_atom()?;
        // unions
        if self.at(&Tok::Pipe) {
            let mut parts = vec![t];
            while self.eat(&Tok::Pipe) {
                parts.push(self.parse_type_atom()?);
            }
            t = TypeExpr::Union(parts);
        }
        Ok(t)
    }

    fn parse_type_atom(&mut self) -> Result<TypeExpr> {
        match self.peek().clone() {
            Tok::Amp => {
                self.bump();
                // `&raw` is handled as a unary op in expressions, not a type.
                let mutable = self.eat(&Tok::Mut);
                if self.at(&Tok::LBracket) {
                    self.bump();
                    let inner = self.parse_type()?;
                    self.expect(&Tok::RBracket, "']'")?;
                    return Ok(TypeExpr::Slice(Box::new(inner)));
                }
                let inner = self.parse_type_atom()?;
                Ok(TypeExpr::Ref {
                    mutable,
                    inner: Box::new(inner),
                })
            }
            Tok::Star => {
                self.bump();
                let mutable = self.eat(&Tok::Mut);
                let inner = self.parse_type_atom()?;
                Ok(TypeExpr::Ptr {
                    mutable,
                    inner: Box::new(inner),
                })
            }
            Tok::LBracket => {
                self.bump();
                let inner = self.parse_type()?;
                let size = if self.eat(&Tok::Semicolon) {
                    match self.bump().tok {
                        Tok::Int(n) => Some(n),
                        _ => None,
                    }
                } else {
                    None
                };
                self.expect(&Tok::RBracket, "']'")?;
                Ok(TypeExpr::Array {
                    inner: Box::new(inner),
                    size,
                })
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(TypeExpr::Unit);
                }
                let mut parts = Vec::new();
                while !self.at(&Tok::RParen) {
                    parts.push(self.parse_type()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "')'")?;
                if parts.len() == 1 {
                    Ok(parts.pop().unwrap())
                } else {
                    Ok(TypeExpr::Tuple(parts))
                }
            }
            Tok::Bang => {
                self.bump();
                Ok(TypeExpr::Never)
            }
            Tok::Async => {
                self.bump();
                Ok(TypeExpr::Async(Box::new(self.parse_type()?)))
            }
            Tok::Fn => {
                self.bump();
                self.expect(&Tok::LParen, "'('")?;
                let mut params = Vec::new();
                while !self.at(&Tok::RParen) {
                    params.push(self.parse_type()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "')'")?;
                let ret = if self.eat(&Tok::Arrow) {
                    self.parse_type()?
                } else {
                    TypeExpr::Unit
                };
                Ok(TypeExpr::Fn {
                    params,
                    ret: Box::new(ret),
                })
            }
            Tok::Nil => {
                self.bump();
                Ok(TypeExpr::Named {
                    name: "nil".into(),
                    args: vec![],
                })
            }
            Tok::SelfKw => {
                self.bump();
                Ok(TypeExpr::Named {
                    name: "Self".into(),
                    args: vec![],
                })
            }
            Tok::Ident(name) => {
                self.bump();
                let mut args = Vec::new();
                if self.at(&Tok::Lt) {
                    self.bump();
                    while !self.at(&Tok::Gt) {
                        args.push(self.parse_type()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::Gt, "'>'")?;
                }
                Ok(TypeExpr::Named { name, args })
            }
            other => self.err(format!("expected type, found {:?}", other)),
        }
    }

    // ---- blocks & statements ----

    fn parse_block(&mut self) -> Result<Block> {
        let pos = self.pos();
        self.expect(&Tok::LBrace, "'{'")?;
        let saved = self.no_struct;
        self.no_struct = false;
        let mut stmts = Vec::new();
        let mut tail = None;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) && !matches!(self.peek(), Tok::Eof) {
            // Item-like statements.
            match self.peek() {
                Tok::Let => {
                    stmts.push(self.parse_let()?);
                }
                Tok::Return => {
                    let p = self.pos();
                    self.bump();
                    let e = if self.stmt_end() {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    stmts.push(Stmt::Return(e, p));
                }
                Tok::Break => {
                    let p = self.pos();
                    self.bump();
                    let e = if self.stmt_end() {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    stmts.push(Stmt::Break(e, p));
                }
                Tok::Continue => {
                    let p = self.pos();
                    self.bump();
                    stmts.push(Stmt::Continue(p));
                }
                Tok::Const => {
                    stmts.push(Stmt::Item(Item::Const(self.parse_const()?)));
                }
                Tok::Fn => {
                    stmts.push(Stmt::Item(Item::Fn(self.parse_fn()?)));
                }
                _ => {
                    let e = self.parse_expr()?;
                    // A trailing expression with no terminator before `}` is the
                    // block's value.
                    if self.at(&Tok::RBrace) {
                        tail = Some(Box::new(e));
                        break;
                    }
                    stmts.push(Stmt::Expr(e));
                }
            }
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        self.no_struct = saved;
        Ok(Block { stmts, tail, pos })
    }

    /// Decide whether a `{` in expression position opens a block or a map/set
    /// literal. `{}` is an empty map; a leading statement keyword or a top-level
    /// `;` means a block; a top-level `:` means a map; a top-level `,` means a
    /// set; a single expression with no separators is treated as a block.
    fn brace_is_block(&self) -> bool {
        let mut j = self.i + 1;
        match self.toks.get(j).map(|t| &t.tok) {
            Some(Tok::RBrace) => return false,
            Some(Tok::Let | Tok::Return | Tok::Break | Tok::Continue | Tok::Const | Tok::Fn) => {
                return true
            }
            _ => {}
        }
        let mut depth = 0i32;
        let mut paren = 0i32;
        let (mut colon, mut comma, mut semi) = (false, false, false);
        while let Some(t) = self.toks.get(j) {
            match &t.tok {
                Tok::LBrace => depth += 1,
                Tok::RBrace => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Tok::LParen | Tok::LBracket => paren += 1,
                Tok::RParen | Tok::RBracket => paren -= 1,
                Tok::Colon if depth == 0 && paren == 0 => colon = true,
                Tok::Comma if depth == 0 && paren == 0 => comma = true,
                Tok::Semicolon if depth == 0 && paren == 0 => semi = true,
                _ => {}
            }
            j += 1;
        }
        if colon {
            return false;
        }
        if semi {
            return true;
        }
        if comma {
            return false;
        }
        true
    }

    fn stmt_end(&self) -> bool {
        matches!(
            self.peek(),
            Tok::Newline | Tok::Semicolon | Tok::RBrace | Tok::Eof
        )
    }

    fn parse_let(&mut self) -> Result<Stmt> {
        let pos = self.pos();
        self.bump();
        let mutable = self.eat(&Tok::Mut);
        let pattern = self.parse_pattern(false)?;
        let ty = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Tok::Eq, "'='")?;
        let value = self.parse_expr()?;
        Ok(Stmt::Let {
            pattern,
            mutable,
            ty,
            value,
            pos,
        })
    }

    // ---- patterns ----

    /// `allow_typed` enables the `name: Type` narrowing pattern, which is only
    /// valid in a `match` arm. In `let`/`for` bindings a `:` is a type
    /// annotation on the binding, not part of the pattern.
    fn parse_pattern(&mut self, allow_typed: bool) -> Result<Pattern> {
        let first = self.parse_pattern_atom(allow_typed)?;
        if self.at(&Tok::Pipe) {
            let mut parts = vec![first];
            while self.eat(&Tok::Pipe) {
                parts.push(self.parse_pattern_atom(allow_typed)?);
            }
            return Ok(Pattern::Or(parts));
        }
        Ok(first)
    }

    fn parse_pattern_atom(&mut self, allow_typed: bool) -> Result<Pattern> {
        match self.peek().clone() {
            Tok::Ident(name) if name == "_" => {
                self.bump();
                Ok(Pattern::Wildcard)
            }
            Tok::Nil => {
                self.bump();
                Ok(Pattern::Nil)
            }
            Tok::True => {
                self.bump();
                Ok(Pattern::Bool(true))
            }
            Tok::False => {
                self.bump();
                Ok(Pattern::Bool(false))
            }
            Tok::Char(c) => {
                self.bump();
                Ok(Pattern::Char(c))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Pattern::Str(s))
            }
            Tok::Int(n) => {
                self.bump();
                if self.eat(&Tok::DotDotEq) {
                    let hi = self.expect_int()?;
                    Ok(Pattern::Range {
                        lo: n,
                        hi,
                        inclusive: true,
                    })
                } else if self.eat(&Tok::DotDot) {
                    let hi = self.expect_int()?;
                    Ok(Pattern::Range {
                        lo: n,
                        hi,
                        inclusive: false,
                    })
                } else {
                    Ok(Pattern::Int(n))
                }
            }
            Tok::Minus => {
                self.bump();
                let n = -self.expect_int()?;
                Ok(Pattern::Int(n))
            }
            Tok::LParen => {
                self.bump();
                let mut parts = Vec::new();
                while !self.at(&Tok::RParen) {
                    parts.push(self.parse_pattern(allow_typed)?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "')'")?;
                Ok(Pattern::Tuple(parts))
            }
            Tok::LBracket => {
                self.bump();
                let mut items = Vec::new();
                let mut rest = None;
                while !self.at(&Tok::RBracket) {
                    if self.eat(&Tok::DotDot) {
                        rest = Some(self.ident().unwrap_or_default());
                        break;
                    }
                    items.push(self.parse_pattern(allow_typed)?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBracket, "']'")?;
                Ok(Pattern::List { items, rest })
            }
            Tok::Ident(name) => {
                self.bump();
                // path: Enum.Variant or Enum::Variant
                let mut path = vec![name.clone()];
                while self.eat(&Tok::Dot) || self.eat(&Tok::ColonColon) {
                    path.push(self.ident()?);
                }
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        args.push(self.parse_pattern(allow_typed)?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "')'")?;
                    return Ok(Pattern::Variant { path, args });
                }
                if self.at(&Tok::LBrace) && !self.no_struct {
                    self.bump();
                    let mut fields = Vec::new();
                    while !self.at(&Tok::RBrace) {
                        fields.push(self.ident()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RBrace, "'}'")?;
                    return Ok(Pattern::Struct {
                        name: path.join("."),
                        fields,
                    });
                }
                // `name: Type` typed pattern, valid only in a match arm.
                if allow_typed && self.eat(&Tok::Colon) {
                    let ty = self.parse_type()?;
                    return Ok(Pattern::Typed { binding: name, ty });
                }
                // `name @ subpattern`
                if self.eat(&Tok::At) {
                    let sub = self.parse_pattern_atom(allow_typed)?;
                    return Ok(Pattern::At(name, Box::new(sub)));
                }
                if path.len() > 1 {
                    Ok(Pattern::Variant { path, args: vec![] })
                } else {
                    Ok(Pattern::Binding(name))
                }
            }
            other => self.err(format!("expected pattern, found {:?}", other)),
        }
    }

    fn expect_int(&mut self) -> Result<i64> {
        match self.bump().tok {
            Tok::Int(n) => Ok(n),
            other => Err(Diagnostic::new(
                Phase::Parse,
                self.pos(),
                format!("expected integer, found {:?}", other),
            )),
        }
    }

    // ---- expressions (Pratt) ----

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_bp(0)
    }

    fn parse_expr_no_struct(&mut self) -> Result<Expr> {
        let saved = self.no_struct;
        self.no_struct = true;
        let e = self.parse_bp(0);
        self.no_struct = saved;
        e
    }

    fn parse_bp(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_prefix()?;

        loop {
            // postfix operators bind tightest
            lhs = self.parse_postfix(lhs)?;

            let op_tok = self.peek().clone();

            // assignment
            if let Some(aop) = assign_op(&op_tok) {
                if min_bp <= 1 {
                    let pos = lhs.pos;
                    self.bump();
                    let value = self.parse_bp(1)?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Assign {
                            target: Box::new(lhs),
                            op: aop,
                            value: Box::new(value),
                        },
                        pos,
                    };
                    continue;
                } else {
                    break;
                }
            }

            // `as` cast binds tighter than the binary operators (just below the
            // prefix operators), so `n as f64 / 2.0` is `(n as f64) / 2.0` and
            // `a + b as u8` is `a + (b as u8)`.
            if matches!(op_tok, Tok::As) {
                if 26 < min_bp {
                    break;
                }
                let pos = lhs.pos;
                self.bump();
                let ty = self.parse_type()?;
                lhs = Expr { id: NodeId::DUMMY,
                    kind: ExprKind::Cast {
                        expr: Box::new(lhs),
                        ty,
                    },
                    pos,
                };
                continue;
            }

            let (l_bp, r_bp, kind) = match infix_bp(&op_tok) {
                Some(v) => v,
                None => break,
            };
            if l_bp < min_bp {
                break;
            }
            let pos = lhs.pos;
            self.bump();

            match kind {
                InfixKind::Coalesce => {
                    let rhs = self.parse_bp(r_bp)?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Coalesce {
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        pos,
                    };
                }
                InfixKind::Range(inclusive) => {
                    let rhs = self.parse_bp(r_bp)?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Range {
                            start: Box::new(lhs),
                            end: Box::new(rhs),
                            inclusive,
                        },
                        pos,
                    };
                }
                InfixKind::Binary(op) => {
                    let rhs = self.parse_bp(r_bp)?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        pos,
                    };
                }
            }
        }
        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        let pos = self.pos();
        let op = match self.peek().clone() {
            Tok::Minus => Some(UnOp::Neg),
            Tok::Bang => Some(UnOp::Not),
            Tok::Tilde => Some(UnOp::BitNot),
            Tok::Star => Some(UnOp::Deref),
            Tok::Amp => {
                self.bump();
                if self.at(&Tok::Ident("raw".into())) {
                    // `&raw expr`
                    self.bump();
                    let expr = self.parse_bp(29)?;
                    return Ok(Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Unary {
                            op: UnOp::RawRef,
                            expr: Box::new(expr),
                        },
                        pos,
                    });
                }
                let op = if self.eat(&Tok::Mut) {
                    UnOp::RefMut
                } else {
                    UnOp::Ref
                };
                let expr = self.parse_bp(29)?;
                return Ok(Expr { id: NodeId::DUMMY,
                    kind: ExprKind::Unary {
                        op,
                        expr: Box::new(expr),
                    },
                    pos,
                });
            }
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let expr = self.parse_bp(29)?;
            return Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                pos,
            });
        }
        self.parse_atom()
    }

    fn parse_postfix(&mut self, mut lhs: Expr) -> Result<Expr> {
        loop {
            let pos = lhs.pos;
            match self.peek() {
                Tok::Dot | Tok::QuestionDot => {
                    let optional = matches!(self.peek(), Tok::QuestionDot);
                    self.bump();
                    // tuple index like `.0`
                    if let Tok::Int(n) = self.peek().clone() {
                        self.bump();
                        lhs = Expr { id: NodeId::DUMMY,
                            kind: ExprKind::Field {
                                recv: Box::new(lhs),
                                optional,
                                name: n.to_string(),
                            },
                            pos,
                        };
                        continue;
                    }
                    let name = self.ident()?;
                    let mut type_args = Vec::new();
                    if self.at(&Tok::ColonColon) && matches!(self.peek_at(1), Tok::Lt) {
                        self.bump();
                        self.bump();
                        while !self.at(&Tok::Gt) {
                            type_args.push(self.parse_type()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                        self.expect(&Tok::Gt, "'>'")?;
                    }
                    if self.at(&Tok::LParen) {
                        let args = self.parse_args()?;
                        lhs = Expr { id: NodeId::DUMMY,
                            kind: ExprKind::MethodCall {
                                recv: Box::new(lhs),
                                optional,
                                method: name,
                                type_args,
                                args,
                            },
                            pos,
                        };
                    } else {
                        lhs = Expr { id: NodeId::DUMMY,
                            kind: ExprKind::Field {
                                recv: Box::new(lhs),
                                optional,
                                name,
                            },
                            pos,
                        };
                    }
                }
                Tok::LParen => {
                    let args = self.parse_args()?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Call {
                            callee: Box::new(lhs),
                            args,
                        },
                        pos,
                    };
                }
                Tok::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect(&Tok::RBracket, "']'")?;
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Index {
                            recv: Box::new(lhs),
                            index: Box::new(index),
                        },
                        pos,
                    };
                }
                Tok::Question => {
                    self.bump();
                    lhs = Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Try(Box::new(lhs)),
                        pos,
                    };
                }
                Tok::ColonColon => {
                    // turbofish on a free function: `f::<T>(...)` or path `A::B`
                    if matches!(self.peek_at(1), Tok::Lt) {
                        self.bump();
                        self.bump();
                        while !self.at(&Tok::Gt) {
                            self.parse_type()?;
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                        self.expect(&Tok::Gt, "'>'")?;
                        // keep lhs as the callee; args follow
                    } else {
                        // path segment
                        self.bump();
                        let seg = self.ident()?;
                        let mut segs = match lhs.kind {
                            ExprKind::Ident(s) => vec![s],
                            ExprKind::Path(p) => p,
                            _ => return self.err("invalid path"),
                        };
                        segs.push(seg);
                        lhs = Expr { id: NodeId::DUMMY,
                            kind: ExprKind::Path(segs),
                            pos,
                        };
                    }
                }
                // Bare turbofish on a callable name: `channel<str>(...)`. Only a
                // `<types>(` shape commits; anything else restores and lets `<`
                // parse as the comparison operator.
                Tok::Lt if matches!(lhs.kind, ExprKind::Ident(_) | ExprKind::Path(_)) => {
                    let save = self.i;
                    if !self.try_bare_turbofish() {
                        self.i = save;
                        break;
                    }
                    // The generic args are erased; the call (`(`) follows.
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    /// Try to consume `<type, ...>` immediately followed by `(`. Returns false
    /// (leaving the cursor wherever it stopped, for the caller to restore) when
    /// the tokens are not a turbofish.
    fn try_bare_turbofish(&mut self) -> bool {
        if !self.eat(&Tok::Lt) {
            return false;
        }
        loop {
            if self.parse_type().is_err() {
                return false;
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if !self.eat(&Tok::Gt) {
            return false;
        }
        self.at(&Tok::LParen)
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>> {
        self.expect(&Tok::LParen, "'('")?;
        let mut args = Vec::new();
        while !self.at(&Tok::RParen) {
            // named arg sugar `name: value` (e.g. channel(capacity: 32)) -> value
            if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Colon) {
                self.bump();
                self.bump();
            }
            args.push(self.parse_expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(args)
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        let pos = self.pos();
        let kind = match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                ExprKind::Int(n)
            }
            Tok::Float(f) => {
                self.bump();
                ExprKind::Float(f)
            }
            Tok::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            Tok::FStr(s) => {
                self.bump();
                ExprKind::FStr(parse_fstring(&s, pos)?)
            }
            Tok::Char(c) => {
                self.bump();
                ExprKind::Char(c)
            }
            Tok::True => {
                self.bump();
                ExprKind::Bool(true)
            }
            Tok::False => {
                self.bump();
                ExprKind::Bool(false)
            }
            Tok::Nil => {
                self.bump();
                ExprKind::Nil
            }
            Tok::SelfKw => {
                self.bump();
                ExprKind::SelfExpr
            }
            Tok::Ident(name) => {
                self.bump();
                // Struct-variant literal: `Enum.Variant { field: value }`.
                if matches!(self.peek(), Tok::Dot) && !self.no_struct {
                    if let Tok::Ident(variant) = self.peek_at(1).clone() {
                        if matches!(self.peek_at(2), Tok::LBrace) {
                            self.bump(); // .
                            self.bump(); // variant
                            return self.parse_struct_lit(variant, pos);
                        }
                    }
                }
                if self.at(&Tok::LBrace) && !self.no_struct && looks_like_struct(self) {
                    return self.parse_struct_lit(name, pos);
                }
                ExprKind::Ident(name)
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Tuple(vec![]),
                        pos,
                    });
                }
                let first = self.parse_expr()?;
                if self.eat(&Tok::Comma) {
                    let mut parts = vec![first];
                    while !self.at(&Tok::RParen) {
                        parts.push(self.parse_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "')'")?;
                    return Ok(Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Tuple(parts),
                        pos,
                    });
                }
                self.expect(&Tok::RParen, "')'")?;
                first.kind
            }
            Tok::LBracket => return self.parse_list(pos),
            Tok::LBrace => {
                if self.brace_is_block() {
                    let b = self.parse_block()?;
                    let bpos = b.pos;
                    return Ok(Expr { id: NodeId::DUMMY,
                        kind: ExprKind::Block(b),
                        pos: bpos,
                    });
                }
                return self.parse_brace_collection(pos);
            }
            Tok::If => return self.parse_if(),
            Tok::Match => return self.parse_match(),
            Tok::Loop => {
                self.bump();
                let body = self.parse_block()?;
                ExprKind::Loop { body }
            }
            Tok::While => return self.parse_while(),
            Tok::For => return self.parse_for(),
            Tok::Pipe | Tok::PipePipe => return self.parse_closure(false),
            Tok::Move => {
                self.bump();
                return self.parse_closure(true);
            }
            Tok::Await => {
                self.bump();
                let e = self.parse_bp(29)?;
                ExprKind::Await(Box::new(e))
            }
            Tok::Spawn => {
                self.bump();
                let body = self.parse_block()?;
                ExprKind::Spawn(body)
            }
            Tok::Unsafe => {
                self.bump();
                let body = self.parse_block()?;
                ExprKind::Unsafe(body)
            }
            Tok::Try => return self.parse_try_catch(),
            other => return self.err(format!("expected expression, found {:?}", other)),
        };
        Ok(Expr { id: NodeId::DUMMY, kind, pos })
    }

    fn parse_list(&mut self, pos: Pos) -> Result<Expr> {
        self.bump(); // [
        if self.eat(&Tok::RBracket) {
            return Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::List(vec![]),
                pos,
            });
        }
        let first = self.parse_expr()?;
        if self.eat(&Tok::Semicolon) {
            let count = self.parse_expr()?;
            self.expect(&Tok::RBracket, "']'")?;
            return Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::ListRepeat {
                    value: Box::new(first),
                    count: Box::new(count),
                },
                pos,
            });
        }
        let mut items = vec![first];
        if self.eat(&Tok::Comma) {
            while !self.at(&Tok::RBracket) {
                items.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBracket, "']'")?;
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::List(items),
            pos,
        })
    }

    /// `{}` is an empty map. `{a: b, ...}` is a map. `{a, b}` is a set.
    fn parse_brace_collection(&mut self, pos: Pos) -> Result<Expr> {
        self.bump(); // {
        if self.eat(&Tok::RBrace) {
            return Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::Map(vec![]),
                pos,
            });
        }
        let first = self.parse_expr()?;
        if self.eat(&Tok::Colon) {
            let v = self.parse_expr()?;
            let mut entries = vec![(first, v)];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBrace) {
                    break;
                }
                let k = self.parse_expr()?;
                self.expect(&Tok::Colon, "':'")?;
                let val = self.parse_expr()?;
                entries.push((k, val));
            }
            self.expect(&Tok::RBrace, "'}'")?;
            Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::Map(entries),
                pos,
            })
        } else {
            let mut items = vec![first];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBrace) {
                    break;
                }
                items.push(self.parse_expr()?);
            }
            self.expect(&Tok::RBrace, "'}'")?;
            Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::Set(items),
                pos,
            })
        }
    }

    fn parse_struct_lit(&mut self, name: String, pos: Pos) -> Result<Expr> {
        self.bump(); // {
        let mut fields = Vec::new();
        let mut spread = None;
        self.skip_terminators();
        while !self.at(&Tok::RBrace) {
            if self.eat(&Tok::DotDot) {
                spread = Some(Box::new(self.parse_expr()?));
                self.skip_terminators();
                break;
            }
            let fname = self.ident()?;
            let value = if self.eat(&Tok::Colon) {
                self.parse_expr()?
            } else {
                // shorthand `{ x }` == `{ x: x }`
                Expr { id: NodeId::DUMMY,
                    kind: ExprKind::Ident(fname.clone()),
                    pos,
                }
            };
            fields.push((fname, value));
            if !self.eat(&Tok::Comma) {
                break;
            }
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::StructLit {
                name,
                fields,
                spread,
            },
            pos,
        })
    }

    fn parse_if(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        let cond = self.parse_expr_no_struct()?;
        let then = self.parse_block()?;
        let els = if self.eat(&Tok::Else) {
            if self.at(&Tok::If) {
                Some(Box::new(self.parse_if()?))
            } else {
                let b = self.parse_block()?;
                let bpos = b.pos;
                Some(Box::new(Expr { id: NodeId::DUMMY,
                    kind: ExprKind::Block(b),
                    pos: bpos,
                }))
            }
        } else {
            None
        };
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::If {
                cond: Box::new(cond),
                then,
                els,
            },
            pos,
        })
    }

    fn parse_while(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        if self.eat(&Tok::Let) {
            let pattern = self.parse_pattern(true)?;
            self.expect(&Tok::Eq, "'='")?;
            let expr = self.parse_expr_no_struct()?;
            let body = self.parse_block()?;
            return Ok(Expr { id: NodeId::DUMMY,
                kind: ExprKind::WhileLet {
                    pattern,
                    expr: Box::new(expr),
                    body,
                },
                pos,
            });
        }
        let cond = self.parse_expr_no_struct()?;
        let body = self.parse_block()?;
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::While {
                cond: Box::new(cond),
                body,
            },
            pos,
        })
    }

    fn parse_for(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        let pattern = self.parse_pattern(false)?;
        self.expect(&Tok::In, "'in'")?;
        let iter = self.parse_expr_no_struct()?;
        let body = self.parse_block()?;
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::For {
                pattern,
                iter: Box::new(iter),
                body,
            },
            pos,
        })
    }

    fn parse_match(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        let scrutinee = self.parse_expr_no_struct()?;
        self.expect(&Tok::LBrace, "'{'")?;
        self.skip_terminators();
        let mut arms = Vec::new();
        while !self.at(&Tok::RBrace) {
            let pattern = self.parse_pattern(false)?;
            let guard = if self.eat(&Tok::If) {
                Some(self.parse_expr_no_struct()?)
            } else {
                None
            };
            self.expect(&Tok::FatArrow, "'=>'")?;
            let body = self.parse_expr()?;
            arms.push(MatchArm {
                pattern,
                guard,
                body,
            });
            self.eat(&Tok::Comma);
            self.skip_terminators();
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            pos,
        })
    }

    fn parse_closure(&mut self, is_move: bool) -> Result<Expr> {
        let pos = self.pos();
        let mut params = Vec::new();
        if self.eat(&Tok::PipePipe) {
            // no params
        } else {
            self.expect(&Tok::Pipe, "'|'")?;
            while !self.at(&Tok::Pipe) {
                let name = self.ident()?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(Param {
                    name,
                    ty,
                    is_self: false,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::Pipe, "'|'")?;
        }
        // optional return type
        if self.eat(&Tok::Arrow) {
            self.parse_type()?;
        }
        let body = if self.at(&Tok::LBrace) {
            let b = self.parse_block()?;
            let bpos = b.pos;
            Expr { id: NodeId::DUMMY,
                kind: ExprKind::Block(b),
                pos: bpos,
            }
        } else {
            self.parse_expr()?
        };
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::Closure {
                params,
                body: Box::new(body),
                is_move,
            },
            pos,
        })
    }

    fn parse_try_catch(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        let body = self.parse_block()?;
        // `catch` and `finally` are not reserved keywords; they arrive as Idents.
        let mut catches = Vec::new();
        let mut finally = None;
        loop {
            if self.at(&Tok::Ident("catch".into())) {
                self.bump();
                let mut binding = None;
                let mut ty = None;
                if let Tok::Ident(n) = self.peek().clone() {
                    self.bump();
                    binding = Some(n);
                    if self.eat(&Tok::Colon) {
                        ty = Some(self.ident()?);
                    }
                }
                let cbody = self.parse_block()?;
                catches.push(CatchArm {
                    binding,
                    ty,
                    body: cbody,
                });
            } else if self.at(&Tok::Ident("finally".into())) {
                self.bump();
                finally = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        Ok(Expr { id: NodeId::DUMMY,
            kind: ExprKind::TryCatch {
                body,
                catches,
                finally,
            },
            pos,
        })
    }
}

fn looks_like_struct(p: &Parser) -> bool {
    // After an identifier, `{` starts a struct literal when the contents look
    // like `field: ...`, `field,`, `..spread`, or `}`. This avoids treating the
    // block of a bare statement as a struct literal in the common cases.
    // peek positions: 0 == `{`
    match p.peek_at(1) {
        Tok::RBrace => true,
        Tok::DotDot => true,
        Tok::Ident(_) => matches!(p.peek_at(2), Tok::Colon | Tok::Comma | Tok::RBrace),
        _ => false,
    }
}

fn parse_fstring(raw: &str, pos: Pos) -> Result<Vec<FStrPart>> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if chars.get(i + 1) == Some(&'{') {
                lit.push('{');
                i += 2;
                continue;
            }
            if !lit.is_empty() {
                parts.push(FStrPart::Lit(std::mem::take(&mut lit)));
            }
            // read until matching `}`, tracking nested braces
            let mut depth = 1;
            let mut inner = String::new();
            i += 1;
            while i < chars.len() && depth > 0 {
                let d = chars[i];
                if d == '{' {
                    depth += 1;
                } else if d == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push(d);
                i += 1;
            }
            i += 1; // skip closing }
                    // split optional format spec on the last `:` not inside brackets
            let (expr_src, spec) = split_format_spec(&inner);
            let expr = parse_expr_str(&expr_src, pos)?;
            parts.push(FStrPart::Expr {
                expr: Box::new(expr),
                spec,
            });
        } else if c == '}' && chars.get(i + 1) == Some(&'}') {
            lit.push('}');
            i += 2;
        } else {
            lit.push(c);
            i += 1;
        }
    }
    if !lit.is_empty() {
        parts.push(FStrPart::Lit(lit));
    }
    Ok(parts)
}

fn split_format_spec(inner: &str) -> (String, Option<String>) {
    // Find a `:` that is not inside (), [], <>, or string quotes.
    let chars: Vec<char> = inner.chars().collect();
    let mut depth = 0i32;
    let mut in_str = false;
    for (idx, &c) in chars.iter().enumerate() {
        match c {
            '"' => in_str = !in_str,
            '(' | '[' | '<' if !in_str => depth += 1,
            ')' | ']' | '>' if !in_str => depth -= 1,
            ':' if !in_str && depth == 0 => {
                let expr = chars[..idx].iter().collect();
                let spec = chars[idx + 1..].iter().collect();
                return (expr, Some(spec));
            }
            _ => {}
        }
    }
    (inner.to_string(), None)
}

fn assign_op(t: &Tok) -> Option<Option<BinOp>> {
    match t {
        Tok::Eq => Some(None),
        Tok::PlusEq => Some(Some(BinOp::Add)),
        Tok::MinusEq => Some(Some(BinOp::Sub)),
        Tok::StarEq => Some(Some(BinOp::Mul)),
        Tok::SlashEq => Some(Some(BinOp::Div)),
        Tok::PercentEq => Some(Some(BinOp::Rem)),
        _ => None,
    }
}

enum InfixKind {
    Binary(BinOp),
    Coalesce,
    Range(bool),
}

fn infix_bp(t: &Tok) -> Option<(u8, u8, InfixKind)> {
    use BinOp::*;
    let v = match t {
        Tok::QuestionQuestion => (5, 6, InfixKind::Coalesce),
        Tok::PipePipe => (7, 8, InfixKind::Binary(Or)),
        Tok::AmpAmp => (9, 10, InfixKind::Binary(And)),
        Tok::EqEq => (11, 12, InfixKind::Binary(Eq)),
        Tok::Ne => (11, 12, InfixKind::Binary(Ne)),
        Tok::Lt => (11, 12, InfixKind::Binary(Lt)),
        Tok::Gt => (11, 12, InfixKind::Binary(Gt)),
        Tok::Le => (11, 12, InfixKind::Binary(Le)),
        Tok::Ge => (11, 12, InfixKind::Binary(Ge)),
        Tok::DotDot => (13, 14, InfixKind::Range(false)),
        Tok::DotDotEq => (13, 14, InfixKind::Range(true)),
        Tok::Pipe => (15, 16, InfixKind::Binary(BitOr)),
        Tok::Caret => (17, 18, InfixKind::Binary(BitXor)),
        Tok::Amp => (19, 20, InfixKind::Binary(BitAnd)),
        Tok::Shl => (21, 22, InfixKind::Binary(Shl)),
        Tok::Shr => (21, 22, InfixKind::Binary(Shr)),
        Tok::Plus => (23, 24, InfixKind::Binary(Add)),
        Tok::Minus => (23, 24, InfixKind::Binary(Sub)),
        Tok::Star => (25, 26, InfixKind::Binary(Mul)),
        Tok::Slash => (25, 26, InfixKind::Binary(Div)),
        Tok::Percent => (25, 26, InfixKind::Binary(Rem)),
        // `**` is right associative.
        Tok::StarStar => (28, 27, InfixKind::Binary(Pow)),
        _ => return None,
    };
    Some(v)
}

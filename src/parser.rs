//! Recursive-descent parser with a Pratt expression core.
//!
//! Newlines: La3 has optional semicolons. Before parsing we drop newlines that
//! cannot terminate a statement (those next to an operator, an open delimiter,
//! a comma, a continuation keyword, and so on). The newlines that survive act
//! as statement terminators inside blocks.

use crate::ast::*;
use crate::diag::{Diagnostic, Phase, Pos, Result};
use crate::lexer::{Lexer, Tok, Token};

mod expr;
mod items;
mod pattern;
mod stmt;
mod types;

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
}

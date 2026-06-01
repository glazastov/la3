//! Parser: blocks, statements, let-bindings. Split out of `parser.rs`; methods are `pub(super)`
//! so the rest of the parser (the `Parser` impl across these modules) can call
//! them via `self`.

use super::*;

impl Parser {
    pub(super) fn parse_block(&mut self) -> Result<Block> {
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
    pub(super) fn brace_is_block(&self) -> bool {
        let mut j = self.i + 1;
        match self.toks.get(j).map(|t| &t.tok) {
            Some(Tok::RBrace) => return false,
            Some(Tok::Let | Tok::Return | Tok::Break | Tok::Continue | Tok::Const | Tok::Fn) => {
                return true;
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

    pub(super) fn stmt_end(&self) -> bool {
        matches!(
            self.peek(),
            Tok::Newline | Tok::Semicolon | Tok::RBrace | Tok::Eof
        )
    }

    pub(super) fn parse_let(&mut self) -> Result<Stmt> {
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

    // `allow_typed` enables the `name: Type` narrowing pattern, which is only
    // valid in a `match` arm. In `let`/`for` bindings a `:` is a type
    // annotation on the binding, not part of the pattern.
}

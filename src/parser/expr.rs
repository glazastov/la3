//! Parser: expressions (Pratt core), f-strings, operator tables. Split out of `parser.rs`; methods are `pub(super)`
//! so the rest of the parser (the `Parser` impl across these modules) can call
//! them via `self`.

use super::*;

impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_bp(0)
    }

    pub(super) fn parse_expr_no_struct(&mut self) -> Result<Expr> {
        let saved = self.no_struct;
        self.no_struct = true;
        let e = self.parse_bp(0);
        self.no_struct = saved;
        e
    }

    pub(super) fn parse_bp(&mut self, min_bp: u8) -> Result<Expr> {
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
                    lhs = Expr {
                        id: NodeId::DUMMY,
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
                lhs = Expr {
                    id: NodeId::DUMMY,
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
                    lhs = Expr {
                        id: NodeId::DUMMY,
                        kind: ExprKind::Coalesce {
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        pos,
                    };
                }
                InfixKind::Range(inclusive) => {
                    let rhs = self.parse_bp(r_bp)?;
                    lhs = Expr {
                        id: NodeId::DUMMY,
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
                    lhs = Expr {
                        id: NodeId::DUMMY,
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

    pub(super) fn parse_prefix(&mut self) -> Result<Expr> {
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
                    return Ok(Expr {
                        id: NodeId::DUMMY,
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
                return Ok(Expr {
                    id: NodeId::DUMMY,
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
            return Ok(Expr {
                id: NodeId::DUMMY,
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                pos,
            });
        }
        self.parse_atom()
    }

    pub(super) fn parse_postfix(&mut self, mut lhs: Expr) -> Result<Expr> {
        loop {
            let pos = lhs.pos;
            match self.peek() {
                Tok::Dot | Tok::QuestionDot => {
                    let optional = matches!(self.peek(), Tok::QuestionDot);
                    self.bump();
                    // tuple index like `.0`
                    if let Tok::Int(n) = self.peek().clone() {
                        self.bump();
                        lhs = Expr {
                            id: NodeId::DUMMY,
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
                        lhs = Expr {
                            id: NodeId::DUMMY,
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
                        lhs = Expr {
                            id: NodeId::DUMMY,
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
                    lhs = Expr {
                        id: NodeId::DUMMY,
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
                    lhs = Expr {
                        id: NodeId::DUMMY,
                        kind: ExprKind::Index {
                            recv: Box::new(lhs),
                            index: Box::new(index),
                        },
                        pos,
                    };
                }
                Tok::Question => {
                    self.bump();
                    lhs = Expr {
                        id: NodeId::DUMMY,
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
                        lhs = Expr {
                            id: NodeId::DUMMY,
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
    pub(super) fn try_bare_turbofish(&mut self) -> bool {
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

    pub(super) fn parse_args(&mut self) -> Result<Vec<Expr>> {
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

    pub(super) fn parse_atom(&mut self) -> Result<Expr> {
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
                    return Ok(Expr {
                        id: NodeId::DUMMY,
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
                    return Ok(Expr {
                        id: NodeId::DUMMY,
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
                    return Ok(Expr {
                        id: NodeId::DUMMY,
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
        Ok(Expr {
            id: NodeId::DUMMY,
            kind,
            pos,
        })
    }

    pub(super) fn parse_list(&mut self, pos: Pos) -> Result<Expr> {
        self.bump(); // [
        if self.eat(&Tok::RBracket) {
            return Ok(Expr {
                id: NodeId::DUMMY,
                kind: ExprKind::List(vec![]),
                pos,
            });
        }
        let first = self.parse_expr()?;
        if self.eat(&Tok::Semicolon) {
            let count = self.parse_expr()?;
            self.expect(&Tok::RBracket, "']'")?;
            return Ok(Expr {
                id: NodeId::DUMMY,
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
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::List(items),
            pos,
        })
    }

    /// `{}` is an empty map. `{a: b, ...}` is a map. `{a, b}` is a set.
    pub(super) fn parse_brace_collection(&mut self, pos: Pos) -> Result<Expr> {
        self.bump(); // {
        if self.eat(&Tok::RBrace) {
            return Ok(Expr {
                id: NodeId::DUMMY,
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
            Ok(Expr {
                id: NodeId::DUMMY,
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
            Ok(Expr {
                id: NodeId::DUMMY,
                kind: ExprKind::Set(items),
                pos,
            })
        }
    }

    pub(super) fn parse_struct_lit(&mut self, name: String, pos: Pos) -> Result<Expr> {
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
                Expr {
                    id: NodeId::DUMMY,
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
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::StructLit {
                name,
                fields,
                spread,
            },
            pos,
        })
    }

    pub(super) fn parse_if(&mut self) -> Result<Expr> {
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
                Some(Box::new(Expr {
                    id: NodeId::DUMMY,
                    kind: ExprKind::Block(b),
                    pos: bpos,
                }))
            }
        } else {
            None
        };
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::If {
                cond: Box::new(cond),
                then,
                els,
            },
            pos,
        })
    }

    pub(super) fn parse_while(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        if self.eat(&Tok::Let) {
            let pattern = self.parse_pattern(true)?;
            self.expect(&Tok::Eq, "'='")?;
            let expr = self.parse_expr_no_struct()?;
            let body = self.parse_block()?;
            return Ok(Expr {
                id: NodeId::DUMMY,
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
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::While {
                cond: Box::new(cond),
                body,
            },
            pos,
        })
    }

    pub(super) fn parse_for(&mut self) -> Result<Expr> {
        let pos = self.pos();
        self.bump();
        let pattern = self.parse_pattern(false)?;
        self.expect(&Tok::In, "'in'")?;
        let iter = self.parse_expr_no_struct()?;
        let body = self.parse_block()?;
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::For {
                pattern,
                iter: Box::new(iter),
                body,
            },
            pos,
        })
    }

    pub(super) fn parse_match(&mut self) -> Result<Expr> {
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
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            pos,
        })
    }

    pub(super) fn parse_closure(&mut self, is_move: bool) -> Result<Expr> {
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
            Expr {
                id: NodeId::DUMMY,
                kind: ExprKind::Block(b),
                pos: bpos,
            }
        } else {
            self.parse_expr()?
        };
        Ok(Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::Closure {
                params,
                body: Box::new(body),
                is_move,
            },
            pos,
        })
    }

    pub(super) fn parse_try_catch(&mut self) -> Result<Expr> {
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
        Ok(Expr {
            id: NodeId::DUMMY,
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

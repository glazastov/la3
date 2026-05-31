//! Parser: patterns. Split out of `parser.rs`; methods are `pub(super)`
//! so the rest of the parser (the `Parser` impl across these modules) can call
//! them via `self`.

use super::*;

impl Parser {
    pub(super) fn parse_pattern(&mut self, allow_typed: bool) -> Result<Pattern> {
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

    pub(super) fn parse_pattern_atom(&mut self, allow_typed: bool) -> Result<Pattern> {
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

    pub(super) fn expect_int(&mut self) -> Result<i64> {
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
}

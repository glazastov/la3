//! Parser: type expressions. Split out of `parser.rs`; methods are `pub(super)`
//! so the rest of the parser (the `Parser` impl across these modules) can call
//! them via `self`.

use super::*;

impl Parser {
    pub(super) fn parse_type(&mut self) -> Result<TypeExpr> {
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

    pub(super) fn parse_type_atom(&mut self) -> Result<TypeExpr> {
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
}

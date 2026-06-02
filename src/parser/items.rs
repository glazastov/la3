//! Parser: items (fn/struct/enum/impl/interface/const/use/type). Split out of `parser.rs`; methods are `pub(super)`
//! so the rest of the parser (the `Parser` impl across these modules) can call
//! them via `self`.

use super::*;

impl Parser {
    pub(super) fn parse_use(&mut self) -> Result<Item> {
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

    pub(super) fn parse_type_alias(&mut self) -> Result<Item> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::Eq, "'='")?;
        let ty = self.parse_type()?;
        Ok(Item::TypeAlias { name, ty })
    }

    /// Parse `<T, U: Bound + Bound>`, keeping each parameter's interface bounds.
    pub(super) fn parse_generics(&mut self) -> Result<Vec<(String, Vec<String>)>> {
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
    pub(super) fn parse_generic_names(&mut self) -> Result<Vec<String>> {
        Ok(self.parse_generics()?.into_iter().map(|(n, _)| n).collect())
    }

    pub(super) fn parse_fn(&mut self) -> Result<FnDecl> {
        let pos = self.pos();
        let is_async = self.eat(&Tok::Async);
        self.expect(&Tok::Fn, "'fn'")?;
        let name = self.ident()?;
        let generics = self.parse_generics()?;
        self.expect(&Tok::LParen, "'('")?;
        let mut params = Vec::new();
        let mut variadic = None;
        let mut self_kind = SelfKind::None;
        while !self.at(&Tok::RParen) {
            if self.eat(&Tok::DotDot) {
                // `...name: T` variadic (spelled `..` here after newline filter)
            }
            // self receivers
            if self.at(&Tok::Amp) {
                self.bump();
                let is_mut = self.eat(&Tok::Mut);
                if self.eat(&Tok::SelfKw) {
                    // `&self` shares, `&mut self` exclusively borrows.
                    self_kind = if is_mut { SelfKind::RefMut } else { SelfKind::Ref };
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
                self_kind = SelfKind::Value; // `mut self` takes the receiver by value
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
                self_kind = SelfKind::Value; // `self` takes the receiver by value
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
            self_kind,
            pos,
        })
    }

    pub(super) fn parse_struct(&mut self) -> Result<StructDecl> {
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

    pub(super) fn parse_enum(&mut self) -> Result<EnumDecl> {
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
                let mut tys = Vec::new();
                while !self.at(&Tok::RParen) {
                    tys.push(self.parse_type()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "')'")?;
                VariantKind::Tuple(tys)
            } else if self.eat(&Tok::LBrace) {
                let mut fields = Vec::new();
                while !self.at(&Tok::RBrace) {
                    let fname = self.ident()?;
                    self.expect(&Tok::Colon, "':'")?;
                    let fty = self.parse_type()?;
                    fields.push((fname, fty));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBrace, "'}'")?;
                VariantKind::Struct(fields)
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

    pub(super) fn parse_impl(&mut self) -> Result<ImplBlock> {
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

    pub(super) fn parse_interface(&mut self) -> Result<InterfaceDecl> {
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

    pub(super) fn parse_const(&mut self) -> Result<ConstDecl> {
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
}

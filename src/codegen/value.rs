//! Terminator lowering and the value layer: rvalues, operands, constants, and
//! binary/unary/cast arithmetic (exact, oracle-matching semantics). Split out of `codegen.rs`.

use super::*;

impl<'a, 'ctx> FnGen<'a, 'ctx> {
    pub(super) fn gen_term(&mut self, term: &Terminator) -> Result<(), String> {
        match term {
            Terminator::Return => {
                match self.slots[0] {
                    Some(slot) => {
                        // `storage_ty` covers both scalar and aggregate (`[N x i8]`)
                        // returns.
                        let ty = storage_ty(self.ctx, &self.local_types[0].clone(), self.oracle)
                            .unwrap();
                        let v = self.b(self.builder.build_load(ty, slot, "ret"))?;
                        self.b(self.builder.build_return(Some(&v)))?;
                    }
                    None => {
                        self.b(self.builder.build_return(None))?;
                    }
                }
            }
            Terminator::Goto(b) => {
                self.b(self
                    .builder
                    .build_unconditional_branch(self.blocks[b.0 as usize]))?;
            }
            Terminator::Unreachable => {
                self.b(self.builder.build_unreachable())?;
            }
            Terminator::Call { func, args, dest } => {
                let name = match func {
                    Operand::Const(Const::Fn(n)) => n,
                    _ => return Err("indirect call survived to codegen".into()),
                };
                // A runtime/stdlib call (`io.*`, f-string `format`, `str(x)`) lowers
                // to its `la3_*` ABI counterpart (Phase 6.1).
                if is_runtime_call(name) {
                    return self.gen_runtime_call(name, args, dest);
                }
                // An enum tuple-variant "constructor" (`Enum.Variant`) is lowered
                // by mirgen as a call; build the variant aggregate inline instead.
                if let Some((_, vidx)) = enum_ctor(name, self.oracle) {
                    if let Some((place, next)) = dest {
                        let (dptr, dty) = self.resolve_place(place)?;
                        self.gen_aggregate(
                            dptr,
                            &dty,
                            &AggregateKind::Variant(String::new(), vidx),
                            args,
                        )?;
                        self.b(self
                            .builder
                            .build_unconditional_branch(self.blocks[next.0 as usize]))?;
                    }
                    return Ok(());
                }
                let callee = *self
                    .decls
                    .get(name)
                    .ok_or_else(|| format!("call to undeclared fn `{name}`"))?;
                // Argument operands, each at its callee parameter type (so a
                // constant gets the right width and an aggregate is passed whole).
                let param_tys = self.sigs.get(name).cloned().unwrap_or_default();
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    let expect = param_tys.get(i).cloned().unwrap_or(Ty::Unknown);
                    let v = self
                        .gen_operand_full(a, &expect)?
                        .ok_or("unit-typed call argument")?;
                    let from = self.operand_ty(a);
                    let v = self.coerce_int_arg(v, from, &expect)?;
                    argv.push(v.into());
                }
                let call = self.b(self.builder.build_call(callee, &argv, "call"))?;
                if let Some((place, next)) = dest {
                    if let Some(v) = call.try_as_basic_value().basic() {
                        // Store the result (scalar or `[N x i8]`) at the dest place.
                        let (dptr, _) = self.resolve_place(place)?;
                        self.b(self.builder.build_store(dptr, v))?;
                    }
                    self.b(self
                        .builder
                        .build_unconditional_branch(self.blocks[next.0 as usize]))?;
                } else {
                    self.b(self.builder.build_unreachable())?;
                }
            }
            Terminator::If {
                cond,
                then_blk,
                else_blk,
            } => {
                let c = self
                    .gen_operand(cond, &Ty::Bool)?
                    .ok_or("unit operand as `if` condition")?;
                self.b(self.builder.build_conditional_branch(
                    c.into_int_value(),
                    self.blocks[then_blk.0 as usize],
                    self.blocks[else_blk.0 as usize],
                ))?;
            }
            Terminator::Switch {
                discr,
                targets,
                default,
            } => {
                // The discriminant is an integer/char/bool value (enum-discriminant
                // switches go through `Rvalue::Discriminant`, still Phase 5.4). Each
                // arm value is a constant of the discriminant's type.
                let dty = self.operand_ty(discr).unwrap_or(Ty::Int(IntKind::I64));
                let dv = self
                    .gen_operand(discr, &dty)?
                    .ok_or("unit operand as switch discriminant")?
                    .into_int_value();
                let int_ty = dv.get_type();
                let cases: Vec<_> = targets
                    .iter()
                    .map(|(v, b)| {
                        (
                            int_ty.const_int(*v as i64 as u64, false),
                            self.blocks[b.0 as usize],
                        )
                    })
                    .collect();
                self.b(self
                    .builder
                    .build_switch(dv, self.blocks[default.0 as usize], &cases))?;
            }
        }
        Ok(())
    }

    // -- rvalues / operands ------------------------------------------------

    /// Compute a **scalar** rvalue's value (the only rvalues that reach here;
    /// aggregate/discriminant rvalues are handled in [`Self::gen_assign`]).
    pub(super) fn gen_scalar_rvalue(
        &mut self,
        rv: &Rvalue,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match rv {
            Rvalue::Use(op) => self
                .gen_operand(op, expected)?
                .ok_or_else(|| "unit value in a scalar assignment".into()),
            Rvalue::Binary(op, a, b) => self.gen_binary(*op, a, b, expected),
            Rvalue::Unary(op, a) => self.gen_unary(*op, a, expected),
            Rvalue::Cast(op, ty) => self.gen_cast(op, ty),
            _ => Err("non-scalar rvalue reached scalar lowering".into()),
        }
    }

    /// Load a **scalar** operand (a scalar place leaf or a constant); `None` for
    /// a unit value.
    pub(super) fn gen_operand(
        &mut self,
        op: &Operand,
        expected: &Ty,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                // A bare unit local holds no value.
                if p.proj.is_empty() && self.slots[p.local.0 as usize].is_none() {
                    return Ok(None);
                }
                let (ptr, lty) = self.resolve_place(p)?;
                let st =
                    scalar_ty(self.ctx, &lty).ok_or("non-scalar operand in a scalar context")?;
                Ok(Some(self.b(self.builder.build_load(st, ptr, "load"))?))
            }
            Operand::Const(c) => self.gen_const(c, expected),
        }
    }

    /// Load an operand's full value — scalar *or* aggregate (`[N x i8]`) — for a
    /// call argument or a return. `None` for unit.
    pub(super) fn gen_operand_full(
        &mut self,
        op: &Operand,
        ty: &Ty,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        if is_scalar(ty) || matches!(ty, Ty::Unit) {
            return self.gen_operand(op, ty);
        }
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (ptr, _) = self.resolve_place(p)?;
                let st = storage_ty(self.ctx, ty, self.oracle)
                    .ok_or("aggregate operand of unknown layout")?;
                Ok(Some(self.b(self.builder.build_load(st, ptr, "aload"))?))
            }
            Operand::Const(_) => Err("aggregate constant operand is unsupported".into()),
        }
    }

    pub(super) fn gen_const(
        &self,
        c: &Const,
        expected: &Ty,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let v = match c {
            Const::Int(v, ty) => {
                // Prefer the contextual width (`expected`) — typeck pinned the
                // literal to it — falling back to the const's own kind.
                let it = match expected {
                    Ty::Int(k) => int_ty(self.ctx, *k),
                    _ => match ty {
                        Ty::Int(k) => int_ty(self.ctx, *k),
                        _ => self.ctx.i32_type(),
                    },
                };
                it.const_int(*v as u64, false).into()
            }
            Const::Float(f) => {
                let ft = match expected {
                    Ty::Float(FloatKind::F32) => self.ctx.f32_type(),
                    _ => self.ctx.f64_type(),
                };
                ft.const_float(*f).into()
            }
            Const::Bool(b) => self.ctx.bool_type().const_int(*b as u64, false).into(),
            Const::Char(ch) => self.ctx.i32_type().const_int(*ch as u64, false).into(),
            Const::Unit | Const::Nil => return Ok(None),
            // A `math` constant (`math.pi`/`e`/`inf`) inlines as an f64 immediate.
            Const::Fn(name) if math_const(name).is_some() => self
                .ctx
                .f64_type()
                .const_float(math_const(name).unwrap())
                .into(),
            Const::Str(_) | Const::Fn(_) => {
                return Err("string/fn constant is not a scalar value here".into());
            }
        };
        Ok(Some(v))
    }

    // -- binary / unary / cast --------------------------------------------

    pub(super) fn gen_binary(
        &mut self,
        op: BinOp,
        a: &Operand,
        b: &Operand,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use BinOp::*;
        // Comparisons and logical connectives produce a bool from operands of
        // their own type; everything else produces a value of `expected`.
        match op {
            Eq | Ne | Lt | Gt | Le | Ge => {
                let ot = self
                    .operand_ty(a)
                    .or_else(|| self.operand_ty(b))
                    .unwrap_or_else(|| {
                        if self.is_float_const(a) || self.is_float_const(b) {
                            Ty::Float(FloatKind::F64)
                        } else {
                            Ty::Int(IntKind::I32)
                        }
                    });
                let la = self
                    .gen_operand(a, &ot)?
                    .ok_or("unit operand in comparison")?;
                let lb = self
                    .gen_operand(b, &ot)?
                    .ok_or("unit operand in comparison")?;
                self.gen_compare(op, la, lb, &ot)
            }
            And | Or => {
                let la = self
                    .gen_operand(a, &Ty::Bool)?
                    .ok_or("unit operand in `&&`/`||`")?;
                let lb = self
                    .gen_operand(b, &Ty::Bool)?
                    .ok_or("unit operand in `&&`/`||`")?;
                let (x, y) = (la.into_int_value(), lb.into_int_value());
                let r = if op == And {
                    self.b(self.builder.build_and(x, y, "and"))?
                } else {
                    self.b(self.builder.build_or(x, y, "or"))?
                };
                Ok(r.into())
            }
            Pow => {
                // `**` always yields f64 (reference §4); convert both operands.
                let fa = self.to_f64(a)?;
                let fb = self.to_f64(b)?;
                let f64t = self.ctx.f64_type();
                let powf = self.decls_pow(f64t);
                let call = self.b(self
                    .builder
                    .build_call(powf, &[fa.into(), fb.into()], "pow"))?;
                Ok(call
                    .try_as_basic_value()
                    .basic()
                    .ok_or("pow returned void")?)
            }
            _ => {
                // Arithmetic / bitwise, computed at `expected`.
                let la = self
                    .gen_operand(a, expected)?
                    .ok_or("unit operand in arithmetic")?;
                let lb = self
                    .gen_operand(b, expected)?
                    .ok_or("unit operand in arithmetic")?;
                match expected {
                    Ty::Float(_) | Ty::FloatLit => {
                        let (x, y) = (la.into_float_value(), lb.into_float_value());
                        let r = match op {
                            Add => self.b(self.builder.build_float_add(x, y, "fadd"))?,
                            Sub => self.b(self.builder.build_float_sub(x, y, "fsub"))?,
                            Mul => self.b(self.builder.build_float_mul(x, y, "fmul"))?,
                            Div => self.b(self.builder.build_float_div(x, y, "fdiv"))?,
                            Rem => self.b(self.builder.build_float_rem(x, y, "frem"))?,
                            _ => return Err(format!("operator {op:?} is not valid on floats")),
                        };
                        Ok(r.into())
                    }
                    _ => {
                        let signed = match expected {
                            Ty::Int(k) => is_signed(*k),
                            _ => true, // IntLit → i32 signed
                        };
                        let (x, y) = (la.into_int_value(), lb.into_int_value());
                        let r = match op {
                            Add => self.b(self.builder.build_int_add(x, y, "add"))?,
                            Sub => self.b(self.builder.build_int_sub(x, y, "sub"))?,
                            Mul => self.b(self.builder.build_int_mul(x, y, "mul"))?,
                            Div if signed => {
                                self.b(self.builder.build_int_signed_div(x, y, "sdiv"))?
                            }
                            Div => self.b(self.builder.build_int_unsigned_div(x, y, "udiv"))?,
                            Rem if signed => {
                                self.b(self.builder.build_int_signed_rem(x, y, "srem"))?
                            }
                            Rem => self.b(self.builder.build_int_unsigned_rem(x, y, "urem"))?,
                            BitAnd => self.b(self.builder.build_and(x, y, "band"))?,
                            BitOr => self.b(self.builder.build_or(x, y, "bor"))?,
                            BitXor => self.b(self.builder.build_xor(x, y, "bxor"))?,
                            Shl => self.b(self.builder.build_left_shift(x, y, "shl"))?,
                            Shr => self.b(self.builder.build_right_shift(x, y, signed, "shr"))?,
                            _ => return Err(format!("operator {op:?} is not valid on integers")),
                        };
                        Ok(r.into())
                    }
                }
            }
        }
    }

    pub(super) fn gen_compare(
        &mut self,
        op: BinOp,
        la: BasicValueEnum<'ctx>,
        lb: BasicValueEnum<'ctx>,
        ot: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use BinOp::*;
        let r = match ot {
            Ty::Float(_) | Ty::FloatLit => {
                let pred = match op {
                    Eq => FloatPredicate::OEQ,
                    Ne => FloatPredicate::UNE,
                    Lt => FloatPredicate::OLT,
                    Gt => FloatPredicate::OGT,
                    Le => FloatPredicate::OLE,
                    Ge => FloatPredicate::OGE,
                    _ => unreachable!(),
                };
                self.b(self.builder.build_float_compare(
                    pred,
                    la.into_float_value(),
                    lb.into_float_value(),
                    "fcmp",
                ))?
            }
            _ => {
                let signed = match ot {
                    Ty::Int(k) => is_signed(*k),
                    _ => true,
                };
                let pred = match (op, signed) {
                    (Eq, _) => IntPredicate::EQ,
                    (Ne, _) => IntPredicate::NE,
                    (Lt, true) => IntPredicate::SLT,
                    (Lt, false) => IntPredicate::ULT,
                    (Gt, true) => IntPredicate::SGT,
                    (Gt, false) => IntPredicate::UGT,
                    (Le, true) => IntPredicate::SLE,
                    (Le, false) => IntPredicate::ULE,
                    (Ge, true) => IntPredicate::SGE,
                    (Ge, false) => IntPredicate::UGE,
                    _ => unreachable!(),
                };
                self.b(self.builder.build_int_compare(
                    pred,
                    la.into_int_value(),
                    lb.into_int_value(),
                    "icmp",
                ))?
            }
        };
        Ok(r.into())
    }

    pub(super) fn gen_unary(
        &mut self,
        op: UnOp,
        a: &Operand,
        expected: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self
            .gen_operand(a, expected)?
            .ok_or("unit operand in unary op")?;
        let r: BasicValueEnum = match op {
            UnOp::Neg => match expected {
                Ty::Float(_) | Ty::FloatLit => self
                    .b(self.builder.build_float_neg(v.into_float_value(), "fneg"))?
                    .into(),
                _ => self
                    .b(self.builder.build_int_neg(v.into_int_value(), "neg"))?
                    .into(),
            },
            // Logical not on a bool and bitwise complement on an int are both
            // LLVM `not` (xor with all-ones / with 1).
            UnOp::Not | UnOp::BitNot => self
                .b(self.builder.build_not(v.into_int_value(), "not"))?
                .into(),
            UnOp::Deref | UnOp::Ref | UnOp::RefMut | UnOp::RawRef => {
                return Err("ref/deref unary survived to codegen (Phase 6)".into());
            }
        };
        Ok(r)
    }

    pub(super) fn gen_cast(
        &mut self,
        op: &Operand,
        target: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Int(IntKind::I32));
        let v = self
            .gen_operand(op, &src_ty)?
            .ok_or("unit operand in cast")?;
        let src_int = matches!(src_ty, Ty::Int(_) | Ty::IntLit | Ty::Char);
        let src_signed = match src_ty {
            Ty::Int(k) => is_signed(k),
            Ty::Char => false, // a Unicode scalar is a non-negative codepoint
            _ => true,
        };
        let r: BasicValueEnum = match target {
            Ty::Int(_) | Ty::Char => {
                let tt = scalar_ty(self.ctx, target).unwrap().into_int_type();
                if src_int {
                    self.b(self.builder.build_int_cast_sign_flag(
                        v.into_int_value(),
                        tt,
                        src_signed,
                        "icast",
                    ))?
                    .into()
                } else {
                    // float → int truncates toward zero (matches the oracle).
                    let signed_target = match target {
                        Ty::Int(k) => is_signed(*k),
                        _ => false, // char
                    };
                    if signed_target {
                        self.b(self.builder.build_float_to_signed_int(
                            v.into_float_value(),
                            tt,
                            "fptosi",
                        ))?
                        .into()
                    } else {
                        self.b(self.builder.build_float_to_unsigned_int(
                            v.into_float_value(),
                            tt,
                            "fptoui",
                        ))?
                        .into()
                    }
                }
            }
            Ty::Float(_) | Ty::FloatLit => {
                let tt = scalar_ty(self.ctx, target).unwrap().into_float_type();
                if src_int {
                    if src_signed {
                        self.b(self.builder.build_signed_int_to_float(
                            v.into_int_value(),
                            tt,
                            "sitofp",
                        ))?
                        .into()
                    } else {
                        self.b(self.builder.build_unsigned_int_to_float(
                            v.into_int_value(),
                            tt,
                            "uitofp",
                        ))?
                        .into()
                    }
                } else {
                    self.b(self
                        .builder
                        .build_float_cast(v.into_float_value(), tt, "fpcast"))?
                        .into()
                }
            }
            _ => {
                return Err(format!(
                    "cast to {} is not a scalar (Phase 5.4+)",
                    crate::ty::display_ty(target)
                ));
            }
        };
        Ok(r)
    }

    /// Convert an operand to `f64` (for `**`): int→float or float→f64.
    pub(super) fn to_f64(&mut self, op: &Operand) -> Result<FloatValue<'ctx>, String> {
        let src_ty = self.operand_ty(op).unwrap_or(Ty::Float(FloatKind::F64));
        let v = self
            .gen_operand(op, &src_ty)?
            .ok_or("unit operand in `**`")?;
        let f64t = self.ctx.f64_type();
        let r = match src_ty {
            Ty::Float(_) | Ty::FloatLit => {
                self.b(self
                    .builder
                    .build_float_cast(v.into_float_value(), f64t, "topf"))?
            }
            Ty::Int(k) if is_signed(k) => {
                self.b(self
                    .builder
                    .build_signed_int_to_float(v.into_int_value(), f64t, "topf"))?
            }
            _ => {
                self.b(self
                    .builder
                    .build_unsigned_int_to_float(v.into_int_value(), f64t, "topf"))?
            }
        };
        Ok(r)
    }

    /// Declare (once) `double @pow(double, double)` (libm, linked via `-lm`).
    pub(super) fn decls_pow(&self, f64t: inkwell::types::FloatType<'ctx>) -> FunctionValue<'ctx> {
        match self.module.get_function("pow") {
            Some(f) => f,
            None => {
                let ty = f64t.fn_type(&[f64t.into(), f64t.into()], false);
                self.module.add_function("pow", ty, None)
            }
        }
    }

    /// The static type of an operand, when known (a place's local type or a
    /// typed constant). `None` for an untyped float constant.
    pub(super) fn operand_ty(&self, op: &Operand) -> Option<Ty> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.place_ty(p),
            Operand::Const(Const::Int(_, ty)) => Some(match ty {
                Ty::Int(_) => ty.clone(),
                _ => Ty::Int(IntKind::I32),
            }),
            Operand::Const(Const::Bool(_)) => Some(Ty::Bool),
            Operand::Const(Const::Char(_)) => Some(Ty::Char),
            // A `math` constant is an f64 immediate.
            Operand::Const(Const::Fn(n)) if math_const(n).is_some() => {
                Some(Ty::Float(FloatKind::F64))
            }
            _ => None,
        }
    }

    pub(super) fn is_float_const(&self, op: &Operand) -> bool {
        matches!(op, Operand::Const(Const::Float(_)))
    }

    /// The leaf type of a place, walking `Field`/`Downcast` projections — the
    /// type-only counterpart of [`Self::resolve_place`] (no IR emitted).
    pub(super) fn place_ty(&self, place: &Place) -> Option<Ty> {
        let mut cur = self.lty(place.local).clone();
        let mut payload: Option<Vec<(u64, Ty)>> = None;
        for p in &place.proj {
            match p {
                Projection::Downcast(v) => {
                    payload = Some(self.oracle.enum_info(&cur)?.variants.get(*v)?.clone());
                }
                Projection::Field(i) => {
                    cur = match payload.take() {
                        Some(fields) => fields.get(*i)?.1.clone(),
                        None => self.oracle.agg_fields(&cur)?.get(*i)?.1.clone(),
                    };
                }
                Projection::Index(_) | Projection::Deref => return None,
            }
        }
        Some(cur)
    }

    /// Map a builder `Result` error to a `String`.
    pub(super) fn b<T>(&self, r: Result<T, inkwell::builder::BuilderError>) -> Result<T, String> {
        r.map_err(|e| format!("LLVM builder error: {e}"))
    }
}

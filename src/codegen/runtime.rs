//! Phase 6.1 runtime lowering: `str` (literal/concat/drop), the `io` writers,
//! f-string `Format`/`str(x)` via `la3_fmt_*`, and call-argument coercion. Split out.

use super::*;

impl<'a, 'ctx> FnGen<'a, 'ctx> {
    // -- Phase 6.1: `str` / `io` / f-string `Format` runtime lowering ----------

    /// Declare (once) a runtime `la3_*` function with its C ABI signature. A
    /// `str`-returning function takes an explicit `out` pointer as its first
    /// argument and returns `void` (the sret convention; see [`str_struct_ty`]).
    pub(super) fn runtime_decl(&self, name: &str) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i64t = self.ctx.i64_type();
        let void = self.ctx.void_type();
        let ty = match name {
            // str-returning (sret): void(out, …).
            "la3_str_from_utf8" => void.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            "la3_str_concat" => void.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            "la3_str_drop" => void.fn_type(&[ptr.into()], false),
            "la3_str_eq" => self
                .ctx
                .bool_type()
                .fn_type(&[ptr.into(), ptr.into()], false),
            "la3_io_print" | "la3_io_println" | "la3_io_eprintln" => {
                void.fn_type(&[ptr.into()], false)
            }
            // la3_fmt_*(out, value, spec_ptr, spec_len) — out is the sret slot.
            "la3_fmt_i64" | "la3_fmt_u64" => {
                void.fn_type(&[ptr.into(), i64t.into(), ptr.into(), i64t.into()], false)
            }
            "la3_fmt_f64" => void.fn_type(
                &[
                    ptr.into(),
                    self.ctx.f64_type().into(),
                    ptr.into(),
                    i64t.into(),
                ],
                false,
            ),
            "la3_fmt_bool" => void.fn_type(
                &[
                    ptr.into(),
                    self.ctx.bool_type().into(),
                    ptr.into(),
                    i64t.into(),
                ],
                false,
            ),
            "la3_fmt_char" => void.fn_type(
                &[
                    ptr.into(),
                    self.ctx.i32_type().into(),
                    ptr.into(),
                    i64t.into(),
                ],
                false,
            ),
            "la3_fmt_str" => {
                void.fn_type(&[ptr.into(), ptr.into(), ptr.into(), i64t.into()], false)
            }
            other => unreachable!("unknown runtime decl `{other}`"),
        };
        self.module.add_function(name, ty, None)
    }

    /// A pointer to a `str` operand's storage. A place yields its slot directly;
    /// a string *literal* operand (e.g. the `"fib("` in a concatenation) is
    /// materialized into a fresh temporary `str` first. (The temporary's buffer
    /// leaks — temp drops are deferred with the CFG dataflow of 3.6/6.2; this
    /// never double-frees and does not affect observable output.)
    pub(super) fn str_ptr(&mut self, op: &Operand) -> Result<PointerValue<'ctx>, String> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => Ok(self.resolve_place(p)?.0),
            Operand::Const(Const::Str(s)) => {
                let slot = self.b(self.builder.build_alloca(str_struct_ty(self.ctx), "strtmp"))?;
                self.gen_str_literal(slot, s)?;
                Ok(slot)
            }
            Operand::Const(_) => Err("non-`str` constant where a `str` was expected".into()),
        }
    }

    /// Lower a `str` destination assignment: a string literal, a move/copy, or a
    /// `+` concatenation.
    pub(super) fn gen_str_assign(
        &mut self,
        dptr: PointerValue<'ctx>,
        rv: &Rvalue,
    ) -> Result<(), String> {
        match rv {
            Rvalue::Use(Operand::Const(Const::Str(s))) => self.gen_str_literal(dptr, s),
            Rvalue::Use(op @ (Operand::Copy(_) | Operand::Move(_))) => {
                let src = self.str_ptr(op)?;
                let n = self.ctx.i64_type().const_int(STR_SIZE, false);
                self.b(self
                    .builder
                    .build_memcpy(dptr, STR_ALIGN as u32, src, STR_ALIGN as u32, n))
                    .map(|_| ())
            }
            Rvalue::Binary(BinOp::Add, a, b) => {
                let ap = self.str_ptr(a)?;
                let bp = self.str_ptr(b)?;
                let f = self.runtime_decl("la3_str_concat");
                self.b(self
                    .builder
                    .build_call(f, &[dptr.into(), ap.into(), bp.into()], ""))?;
                Ok(())
            }
            _ => Err("unsupported `str` rvalue (only literal/move/concat)".into()),
        }
    }

    /// Build a string literal into `dptr` via `la3_str_from_utf8(out, bytes, len)`.
    pub(super) fn gen_str_literal(
        &mut self,
        dptr: PointerValue<'ctx>,
        s: &str,
    ) -> Result<(), String> {
        let (data, len) = self.str_bytes(s)?;
        let f = self.runtime_decl("la3_str_from_utf8");
        self.b(self
            .builder
            .build_call(f, &[dptr.into(), data.into(), len.into()], ""))?;
        Ok(())
    }

    /// A pointer to the UTF-8 bytes of `s` (a private global) plus its byte length.
    pub(super) fn str_bytes(
        &self,
        s: &str,
    ) -> Result<(PointerValue<'ctx>, IntValue<'ctx>), String> {
        let g = self
            .b(self.builder.build_global_string_ptr(s, "str"))?
            .as_pointer_value();
        let len = self.ctx.i64_type().const_int(s.len() as u64, false);
        Ok((g, len))
    }

    /// Lower a runtime/stdlib call (`io.*`, f-string `format`, `str(x)`).
    pub(super) fn gen_runtime_call(
        &mut self,
        name: &str,
        args: &[Operand],
        dest: &Option<(Place, crate::mir::BlockId)>,
    ) -> Result<(), String> {
        match name {
            "io.print" | "io.println" | "io.eprintln" => {
                let sym = match name {
                    "io.print" => "la3_io_print",
                    "io.println" => "la3_io_println",
                    _ => "la3_io_eprintln",
                };
                let p = self.str_ptr(&args[0])?;
                let f = self.runtime_decl(sym);
                self.b(self.builder.build_call(f, &[p.into()], ""))?;
            }
            // `format`/`str(x)`: render `args[0]` (with optional `:spec` in
            // `args[1]`) into the destination `str`.
            "std::format" | "str" => {
                let (dptr, _) = self
                    .resolve_place(&dest.as_ref().ok_or("format call without a destination")?.0)?;
                self.gen_format(dptr, &args[0], args.get(1))?;
            }
            other => return Err(format!("unknown runtime call `{other}`")),
        }
        if let Some((_, next)) = dest {
            self.b(self
                .builder
                .build_unconditional_branch(self.blocks[next.0 as usize]))?;
        } else {
            self.b(self.builder.build_unreachable())?;
        }
        Ok(())
    }

    /// Render `value` (with optional `:spec`) into the `str` at `dptr`, selecting
    /// the typed `la3_fmt_*` by the value's type (mirroring the interpreter).
    pub(super) fn gen_format(
        &mut self,
        dptr: PointerValue<'ctx>,
        value: &Operand,
        spec: Option<&Operand>,
    ) -> Result<(), String> {
        let (sp, sl) = self.spec_args(spec)?;
        let vty = self.operand_ty(value).unwrap_or_else(|| {
            if self.is_float_const(value) {
                Ty::Float(FloatKind::F64)
            } else {
                Ty::Int(IntKind::I32)
            }
        });
        // (runtime fn, the value argument coerced to the fn's parameter type)
        let (sym, varg): (&str, BasicMetadataValueEnum) = match &vty {
            Ty::Str => {
                let p = self.str_ptr(value)?;
                ("la3_fmt_str", p.into())
            }
            Ty::Float(_) | Ty::FloatLit => {
                let v = self.to_f64(value)?;
                ("la3_fmt_f64", v.into())
            }
            Ty::Bool => {
                let v = self
                    .gen_operand(value, &Ty::Bool)?
                    .ok_or("unit in format")?;
                ("la3_fmt_bool", v.into())
            }
            Ty::Char => {
                let v = self
                    .gen_operand(value, &Ty::Char)?
                    .ok_or("unit in format")?;
                ("la3_fmt_char", v.into())
            }
            Ty::Int(k) => {
                let signed = is_signed(*k);
                let v = self.load_int_as_i64(value, &vty, signed)?;
                (if signed { "la3_fmt_i64" } else { "la3_fmt_u64" }, v.into())
            }
            _ => {
                let v = self.load_int_as_i64(value, &Ty::Int(IntKind::I32), true)?;
                ("la3_fmt_i64", v.into())
            }
        };
        let f = self.runtime_decl(sym);
        self.b(self
            .builder
            .build_call(f, &[dptr.into(), varg, sp.into(), sl.into()], ""))?;
        Ok(())
    }

    /// The `(spec_ptr, spec_len)` pair a `la3_fmt_*` call takes: the literal spec
    /// bytes, or `(null, 0)` for the default rendering.
    pub(super) fn spec_args(
        &self,
        spec: Option<&Operand>,
    ) -> Result<(PointerValue<'ctx>, IntValue<'ctx>), String> {
        match spec {
            Some(Operand::Const(Const::Str(s))) => self.str_bytes(s),
            None => Ok((
                self.ctx.ptr_type(AddressSpace::default()).const_null(),
                self.ctx.i64_type().const_zero(),
            )),
            Some(_) => Err("format spec is not a string literal".into()),
        }
    }

    /// Coerce a scalar call argument to its parameter type when lenient inference
    /// left a different integer width (e.g. an `i32` loop variable passed to an
    /// `i64` parameter, which the interpreter accepts since it computes in `i64`).
    /// Match the interpreter by sign/zero-extending or truncating to the declared
    /// width; non-int or already-matching values pass through unchanged.
    pub(super) fn coerce_int_arg(
        &self,
        v: BasicValueEnum<'ctx>,
        from: Option<Ty>,
        to: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if let (BasicValueEnum::IntValue(iv), Ty::Int(tk)) = (v, to) {
            let tt = int_ty(self.ctx, *tk);
            if iv.get_type().get_bit_width() != tt.get_bit_width() {
                let signed = matches!(from, Some(Ty::Int(k)) if is_signed(k))
                    || !matches!(from, Some(Ty::Int(_)));
                return Ok(self
                    .b(self
                        .builder
                        .build_int_cast_sign_flag(iv, tt, signed, "argcast"))?
                    .into());
            }
        }
        Ok(v)
    }

    /// Load an integer operand and sign/zero-extend it to `i64` (the width the
    /// `la3_fmt_i64`/`la3_fmt_u64` formatters take).
    pub(super) fn load_int_as_i64(
        &mut self,
        op: &Operand,
        ty: &Ty,
        signed: bool,
    ) -> Result<IntValue<'ctx>, String> {
        let v = self
            .gen_operand(op, ty)?
            .ok_or("unit in format")?
            .into_int_value();
        let i64t = self.ctx.i64_type();
        Ok(self.b(self
            .builder
            .build_int_cast_sign_flag(v, i64t, signed, "toi64"))?)
    }
}

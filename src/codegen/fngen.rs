//! Per-function translation: the entry/blocks driver, statement and place
//! lowering, and aggregate (struct/tuple/enum) construction. Split out of `codegen.rs`.

use super::*;

impl<'a, 'ctx> FnGen<'a, 'ctx> {
    /// The (inference-resolved) type of a local.
    pub(super) fn lty(&self, l: crate::mir::Local) -> &Ty {
        &self.local_types[l.0 as usize]
    }

    pub(super) fn gen_fn(&mut self) -> Result<(), String> {
        // Entry block: stack slots for every local, then the params stored in.
        let entry = self.ctx.append_basic_block(self.fval, "entry");
        self.builder.position_at_end(entry);
        for ty in self.local_types.clone() {
            let slot = match storage_ty(self.ctx, &ty, self.oracle) {
                Some(t) => {
                    let ptr = self.b(self.builder.build_alloca(t, "slot"))?;
                    // Aggregate storage is `[N x i8]` (align 1 by type), so set the
                    // alloca's alignment to the value's real alignment.
                    if !is_scalar(&ty) {
                        if let Some((_, align)) = self.oracle.size_align(&ty) {
                            if let Some(inst) = ptr.as_instruction() {
                                let _ = inst.set_alignment(align.max(1) as u32);
                            }
                        }
                    }
                    Some(ptr)
                }
                None => None,
            };
            self.slots.push(slot);
        }
        for i in 1..=self.f.arg_count {
            if let Some(slot) = self.slots[i] {
                let p = self.fval.get_nth_param((i - 1) as u32).unwrap();
                self.b(self.builder.build_store(slot, p))?;
            }
        }

        for i in 0..self.f.blocks.len() {
            self.blocks
                .push(self.ctx.append_basic_block(self.fval, &format!("bb{i}")));
        }
        self.b(self.builder.build_unconditional_branch(self.blocks[0]))?;

        for (i, blk) in self.f.blocks.iter().enumerate() {
            self.builder.position_at_end(self.blocks[i]);
            self.gen_block(blk)?;
        }
        Ok(())
    }

    pub(super) fn gen_block(&mut self, blk: &BasicBlock) -> Result<(), String> {
        for s in &blk.stmts {
            match s {
                Statement::Assign(place, rv) => self.gen_assign(place, rv)?,
                // Dropping a `str` runs its runtime drop glue (frees the buffer);
                // scalars/flat aggregates own no heap, so their drops are no-ops.
                Statement::Drop(p) => {
                    if self.place_ty(p).as_ref().is_some_and(is_str) {
                        let ptr = self.resolve_place(p)?.0;
                        let f = self.runtime_decl("la3_str_drop");
                        self.b(self.builder.build_call(f, &[ptr.into()], ""))?;
                    }
                }
                Statement::StorageLive(_) | Statement::StorageDead(_) | Statement::Nop => {}
            }
        }
        self.gen_term(&blk.term)
    }

    /// `place = rvalue`, dispatching on whether the destination is a scalar or an
    /// aggregate (built/copied through its byte storage).
    pub(super) fn gen_assign(&mut self, place: &Place, rv: &Rvalue) -> Result<(), String> {
        // A unit-typed destination has no storage; rvalues are side-effect-free
        // (calls are terminators), so the assignment is a no-op.
        if place.proj.is_empty() && self.slots[place.local.0 as usize].is_none() {
            return Ok(());
        }
        let (dptr, dty) = self.resolve_place(place)?;
        // `str` destinations (literal / move / concatenation) lower against the
        // runtime; the call-produced ones (`format`/`str(x)`) flow through the
        // call terminator, not here.
        if is_str(&dty) {
            return self.gen_str_assign(dptr, rv);
        }
        match rv {
            // Build a tuple/struct/enum value directly into the destination.
            Rvalue::Aggregate(kind, ops) => self.gen_aggregate(dptr, &dty, kind, ops),
            // Enum tag → an integer scalar (zero-extended to the dest width).
            Rvalue::Discriminant(ep) => {
                let v = self.gen_discriminant(ep, &dty)?;
                self.b(self.builder.build_store(dptr, v))?;
                Ok(())
            }
            // A whole-aggregate move/copy is a byte copy of the storage.
            Rvalue::Use(op) if !is_scalar(&dty) => self.gen_aggregate_copy(dptr, &dty, op),
            // Everything else yields a scalar.
            _ => {
                let v = self.gen_scalar_rvalue(rv, &dty)?;
                self.b(self.builder.build_store(dptr, v))?;
                Ok(())
            }
        }
    }

    /// Resolve a [`Place`] to the pointer at its leaf and the leaf's type,
    /// walking `Field`/`Downcast` projections as byte offsets (the layout the
    /// oracle computes; same as the by-value layout the rest of the compiler
    /// uses).
    pub(super) fn resolve_place(&self, place: &Place) -> Result<(PointerValue<'ctx>, Ty), String> {
        let base = self.slots[place.local.0 as usize].ok_or("place rooted at a unit local")?;
        let mut cur_ty = self.lty(place.local).clone();
        let mut offset: u64 = 0;
        // After a `Downcast`, field indices address the chosen variant's payload.
        let mut payload: Option<Vec<(u64, Ty)>> = None;
        for p in &place.proj {
            match p {
                Projection::Downcast(v) => {
                    let info = self
                        .oracle
                        .enum_info(&cur_ty)
                        .ok_or("downcast on a non-enum")?;
                    offset += info.payload_offset;
                    payload = Some(
                        info.variants
                            .get(*v)
                            .cloned()
                            .ok_or("variant index out of range")?,
                    );
                }
                Projection::Field(i) => {
                    if let Some(fields) = payload.take() {
                        let (foff, fty) = fields
                            .get(*i)
                            .cloned()
                            .ok_or("payload field index out of range")?;
                        offset += foff;
                        cur_ty = fty;
                    } else {
                        let fields = self
                            .oracle
                            .agg_fields(&cur_ty)
                            .ok_or("field access on a non-aggregate")?;
                        let (foff, fty) =
                            fields.get(*i).cloned().ok_or("field index out of range")?;
                        offset += foff;
                        cur_ty = fty;
                    }
                }
                Projection::Index(_) | Projection::Deref => {
                    return Err("index/deref projection survived to codegen".into());
                }
            }
        }
        let ptr = if offset == 0 {
            base
        } else {
            let i8t = self.ctx.i8_type();
            let idx = self.ctx.i64_type().const_int(offset, false);
            self.b(unsafe { self.builder.build_in_bounds_gep(i8t, base, &[idx], "field") })?
        };
        Ok((ptr, cur_ty))
    }

    /// Construct a tuple/struct/enum value into `dptr` (the destination storage).
    pub(super) fn gen_aggregate(
        &mut self,
        dptr: PointerValue<'ctx>,
        dty: &Ty,
        kind: &AggregateKind,
        ops: &[Operand],
    ) -> Result<(), String> {
        match kind {
            AggregateKind::Tuple | AggregateKind::Struct(_) => {
                let fields = self
                    .oracle
                    .agg_fields(dty)
                    .ok_or("aggregate construction of a non-aggregate")?;
                for (op, (off, fty)) in ops.iter().zip(fields) {
                    self.store_scalar_at(dptr, off, op, &fty)?;
                }
                Ok(())
            }
            AggregateKind::Variant(_, vidx) => {
                let info = self.oracle.enum_info(dty).ok_or("variant of a non-enum")?;
                // Store the discriminant tag at offset 0.
                let tag_ty = self.int_of_bytes(info.tag_size);
                let tagv = tag_ty.const_int(*vidx as u64, false);
                self.b(self.builder.build_store(dptr, tagv))?;
                // Store each payload field at payload_offset + its offset.
                let var = info
                    .variants
                    .get(*vidx)
                    .cloned()
                    .ok_or("variant index out of range")?;
                for (op, (off, fty)) in ops.iter().zip(var) {
                    self.store_scalar_at(dptr, info.payload_offset + off, op, &fty)?;
                }
                Ok(())
            }
            AggregateKind::Array | AggregateKind::Closure(_) => {
                Err("array/closure aggregate survived to codegen".into())
            }
        }
    }

    /// Store a scalar operand `op` (of type `fty`) at byte `offset` from `base`.
    pub(super) fn store_scalar_at(
        &mut self,
        base: PointerValue<'ctx>,
        offset: u64,
        op: &Operand,
        fty: &Ty,
    ) -> Result<(), String> {
        let v = self
            .gen_operand(op, fty)?
            .ok_or("unit value stored into an aggregate field")?;
        let ptr = self.gep_byte(base, offset)?;
        self.b(self.builder.build_store(ptr, v))?;
        Ok(())
    }

    /// Copy a whole aggregate value (a `move`/`copy` of one place into another).
    pub(super) fn gen_aggregate_copy(
        &mut self,
        dptr: PointerValue<'ctx>,
        dty: &Ty,
        op: &Operand,
    ) -> Result<(), String> {
        let src = match op {
            Operand::Copy(p) | Operand::Move(p) => self.resolve_place(p)?.0,
            Operand::Const(_) => return Err("aggregate constant is not supported".into()),
        };
        let (size, align) = self
            .oracle
            .size_align(dty)
            .ok_or("aggregate of unknown size")?;
        let n = self.ctx.i64_type().const_int(size, false);
        self.b(self
            .builder
            .build_memcpy(dptr, align.max(1) as u32, src, align.max(1) as u32, n))
            .map(|_| ())
    }

    /// Read an enum's discriminant tag and zero-extend it to `dty` (an integer).
    pub(super) fn gen_discriminant(
        &mut self,
        ep: &Place,
        dty: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let (eptr, ety) = self.resolve_place(ep)?;
        let info = self
            .oracle
            .enum_info(&ety)
            .ok_or("discriminant of a non-enum")?;
        let tag_ty = self.int_of_bytes(info.tag_size);
        // The tag is at offset 0 of the enum storage.
        let raw = self
            .b(self.builder.build_load(tag_ty, eptr, "tag"))?
            .into_int_value();
        let dest_ty = scalar_ty(self.ctx, dty)
            .ok_or("discriminant destination is not a scalar")?
            .into_int_type();
        Ok(self
            .b(self.builder.build_int_z_extend(raw, dest_ty, "tagext"))?
            .into())
    }

    /// An LLVM integer type of `bytes` bytes (1/2/4) for an enum tag.
    pub(super) fn int_of_bytes(&self, bytes: u64) -> IntType<'ctx> {
        match bytes {
            1 => self.ctx.i8_type(),
            2 => self.ctx.i16_type(),
            _ => self.ctx.i32_type(),
        }
    }

    /// GEP `base` (treated as `i8*`) by `offset` bytes.
    pub(super) fn gep_byte(
        &self,
        base: PointerValue<'ctx>,
        offset: u64,
    ) -> Result<PointerValue<'ctx>, String> {
        if offset == 0 {
            return Ok(base);
        }
        let i8t = self.ctx.i8_type();
        let idx = self.ctx.i64_type().const_int(offset, false);
        self.b(unsafe { self.builder.build_in_bounds_gep(i8t, base, &[idx], "off") })
    }
}

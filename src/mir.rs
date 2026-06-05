//! MIR — a control-flow graph of basic blocks with explicit temporaries and
//! typed locals. This is **Phase 3**, the layer where every hard lowering lives
//! (monomorphization, match decision trees, closure conversion, ownership/drop
//! insertion), so the LLVM back-end (Phase 5) stays a thin, mechanical
//! MIR→IR translation.
//!
//! The shape is deliberately Rust-MIR-like (the plan's stated model): a function
//! is a list of **typed locals** plus a vector of **basic blocks**, each a run of
//! [`Statement`]s ending in exactly one [`Terminator`]. Values flow through
//! [`Place`]s (a local plus projections) and [`Operand`]s (`copy`/`move`/
//! constant); computation is [`Rvalue`]. Ownership is explicit: an [`Operand`] is
//! `move` or `copy`, and [`Statement::Drop`] marks a deterministic drop point.
//!
//! **Subpart 3.1 (this) defines the data model only** — the constructors, a
//! builder, and a printer, exercised by hand-built MIR in the unit tests. The
//! HIR→MIR lowering and the `la3 mir` command begin at 3.6 (control flow), with
//! the other transformations filling in 3.2–3.5/3.7. Like [`crate::ast`], the
//! module opts out of `dead_code`: the model is built up across the whole phase,
//! so not every field/variant is wired to a producer until a later subpart.
#![allow(dead_code)]

use crate::ast::{BinOp, BindingId, UnOp};
use crate::ty::{Ty, display_ty};

// ---------------------------------------------------------------------------
// Program / functions / locals
// ---------------------------------------------------------------------------

pub(crate) struct MirProgram {
    pub fns: Vec<MirFn>,
}

/// A function lowered to a CFG. Still generic before monomorphization (3.2):
/// `Ty::Param` may appear in `locals`. By convention local `_0` is the return
/// slot and `_1..=params` are the arguments, mirroring Rust's MIR numbering.
pub(crate) struct MirFn {
    pub name: String,
    /// The nominal type this is a method of (`impl Type`), or `None` for a free
    /// function — kept until monomorphization/mangling assigns a final symbol.
    pub owner: Option<String>,
    /// All locals, indexed by [`Local`]`.0`. `locals[0]` is the return place.
    pub locals: Vec<LocalDecl>,
    /// Number of argument locals (they are `_1..=arg_count`).
    pub arg_count: usize,
    /// Basic blocks, indexed by [`BlockId`]`.0`; `blocks[0]` is the entry.
    pub blocks: Vec<BasicBlock>,
}

impl MirFn {
    pub fn return_local() -> Local {
        Local(0)
    }
    pub fn entry() -> BlockId {
        BlockId(0)
    }
    pub fn local_ty(&self, l: Local) -> &Ty {
        &self.locals[l.0 as usize].ty
    }
}

/// A typed slot. Every value in MIR lives in a local; temporaries introduced by
/// lowering are locals just like user bindings and parameters.
pub(crate) struct LocalDecl {
    pub ty: Ty,
    pub kind: LocalKind,
    /// The HIR binding this local came from, for user locals/args (debugging).
    pub source: Option<BindingId>,
    /// A human name for dumps (`a`, `self`, …); temporaries have none.
    pub name: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LocalKind {
    /// `_0`, the return slot.
    Return,
    /// A parameter.
    Arg,
    /// A user `let`/pattern binding.
    User,
    /// A compiler-introduced temporary.
    Temp,
}

/// Index of a [`LocalDecl`] within a [`MirFn`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Local(pub u32);

/// Index of a [`BasicBlock`] within a [`MirFn`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BlockId(pub u32);

// ---------------------------------------------------------------------------
// Blocks, statements, terminators
// ---------------------------------------------------------------------------

/// A straight-line run of statements terminated by exactly one control-flow
/// [`Terminator`]. This single-entry/single-exit shape is what makes the later
/// passes (drop insertion, borrow liveness, codegen) mechanical.
pub(crate) struct BasicBlock {
    pub stmts: Vec<Statement>,
    pub term: Terminator,
}

pub(crate) enum Statement {
    /// `place = rvalue`.
    Assign(Place, Rvalue),
    /// Deterministic drop of an owned value. **Inserted by ownership lowering
    /// (3.5)** at end-of-scope / last-use, honouring the Phase 1.6.5 contract;
    /// 3.1 only defines the point.
    Drop(Place),
    /// Marks a local as (re)initialized / live without computing a value — a hook
    /// for storage liveness and conditional drop flags later.
    StorageLive(Local),
    StorageDead(Local),
    /// No-op (placeholder / removed statement).
    Nop,
}

/// The single exit of a basic block.
pub(crate) enum Terminator {
    /// Unconditional jump.
    Goto(BlockId),
    /// Return to the caller (the value is in `_0`).
    Return,
    /// Two-way branch on a `bool` operand.
    If {
        cond: Operand,
        then_blk: BlockId,
        else_blk: BlockId,
    },
    /// Multi-way branch on an integer value or an enum **discriminant** — the
    /// substrate for match decision trees (3.3). `targets` are exact-value arms;
    /// anything else goes to `default`.
    Switch {
        discr: Operand,
        targets: Vec<(i128, BlockId)>,
        default: BlockId,
    },
    /// Call `func(args)`; on return, write the result into `dest.0` and jump to
    /// `dest.1`. `dest` is `None` for a diverging/never-returning call.
    Call {
        func: Operand,
        args: Vec<Operand>,
        dest: Option<(Place, BlockId)>,
    },
    /// Statically unreachable (e.g. after a diverging expression).
    Unreachable,
}

// ---------------------------------------------------------------------------
// Places, operands, rvalues
// ---------------------------------------------------------------------------

/// A memory location: a root [`Local`] plus a path of projections. `&u.name`,
/// `a[i]`, `*p`, and an enum-variant downcast are all places.
#[derive(Clone)]
pub(crate) struct Place {
    pub local: Local,
    pub proj: Vec<Projection>,
}

impl Place {
    pub fn local(local: Local) -> Place {
        Place {
            local,
            proj: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Projection {
    /// `.0` / `.field` — tuple or struct field by index.
    Field(usize),
    /// `[i]` — index by a local holding the index value.
    Index(Local),
    /// `*p` — dereference a reference or pointer.
    Deref,
    /// Downcast to an enum variant (read its payload), for match lowering.
    Downcast(usize),
}

/// An argument to an rvalue: a value read by `copy` or `move` from a place, or a
/// constant. The `copy`/`move` distinction is the ownership information MIR 3.5
/// threads (a `move` consumes the source; a `copy` leaves it usable).
pub(crate) enum Operand {
    Copy(Place),
    Move(Place),
    Const(Const),
}

pub(crate) enum Const {
    Int(i64, Ty),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(String),
    Nil,
    Unit,
    /// A reference to a named function/global used as a call target.
    Fn(String),
}

/// The right-hand side of an [`Statement::Assign`].
pub(crate) enum Rvalue {
    /// Just move/copy/const a value.
    Use(Operand),
    /// `a <op> b`.
    Binary(BinOp, Operand, Operand),
    /// `<op> a`.
    Unary(UnOp, Operand),
    /// `a as T`.
    Cast(Operand, Ty),
    /// `&place` / `&mut place` (mutability erased per the Phase 2 `Ty` decision).
    Ref(Place),
    /// Read the **discriminant** (variant tag) of an enum value as an integer —
    /// the value a `match` decision tree (3.3) switches on. The companion of
    /// [`Projection::Downcast`], which reads a chosen variant's payload.
    Discriminant(Place),
    /// Build an aggregate value from its fields.
    Aggregate(AggregateKind, Vec<Operand>),
}

pub(crate) enum AggregateKind {
    Tuple,
    Array,
    Struct(String),
    /// `enum name`, variant index — e.g. `Some(x)` / `Ok(x)`.
    Variant(String, usize),
    /// A closure value `{fn ptr, env}` (Phase 3.4 closure conversion). The
    /// `String` is the lifted function's symbol; the operands are the captured
    /// environment, in capture order (the lifted function receives them packed as
    /// its first parameter, an env tuple).
    Closure(String),
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Incremental construction of a [`MirFn`]: allocate locals and blocks, append
/// statements, and set terminators. The HIR→MIR lowering (3.6+) drives this; the
/// 3.1 unit tests use it directly to build MIR by hand.
pub(crate) struct MirBuilder {
    name: String,
    owner: Option<String>,
    locals: Vec<LocalDecl>,
    arg_count: usize,
    blocks: Vec<BasicBlock>,
}

impl MirBuilder {
    /// Start a function. `ret` is the return type (`_0`); each `(ty, name)` in
    /// `args` becomes an argument local in order (`_1`, `_2`, …).
    pub fn new(
        name: impl Into<String>,
        owner: Option<String>,
        ret: Ty,
        args: Vec<(Ty, Option<String>)>,
    ) -> Self {
        let mut locals = vec![LocalDecl {
            ty: ret,
            kind: LocalKind::Return,
            source: None,
            name: None,
        }];
        for (ty, name) in &args {
            locals.push(LocalDecl {
                ty: ty.clone(),
                kind: LocalKind::Arg,
                source: None,
                name: name.clone(),
            });
        }
        MirBuilder {
            name: name.into(),
            owner,
            arg_count: args.len(),
            locals,
            blocks: Vec::new(),
        }
    }

    /// Allocate a new local and return its index.
    pub fn new_local(&mut self, ty: Ty, kind: LocalKind, name: Option<String>) -> Local {
        let id = Local(self.locals.len() as u32);
        self.locals.push(LocalDecl {
            ty,
            kind,
            source: None,
            name,
        });
        id
    }

    /// Allocate a fresh temporary local.
    pub fn new_temp(&mut self, ty: Ty) -> Local {
        self.new_local(ty, LocalKind::Temp, None)
    }

    /// Reserve a basic block (initially unreachable); fill it via [`Self::block`].
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlock {
            stmts: Vec::new(),
            term: Terminator::Unreachable,
        });
        id
    }

    pub fn block(&mut self, b: BlockId) -> &mut BasicBlock {
        &mut self.blocks[b.0 as usize]
    }

    pub fn push_stmt(&mut self, b: BlockId, s: Statement) {
        self.blocks[b.0 as usize].stmts.push(s);
    }

    pub fn set_term(&mut self, b: BlockId, t: Terminator) {
        self.blocks[b.0 as usize].term = t;
    }

    pub fn finish(self) -> MirFn {
        MirFn {
            name: self.name,
            owner: self.owner,
            locals: self.locals,
            arg_count: self.arg_count,
            blocks: self.blocks,
        }
    }
}

// ---------------------------------------------------------------------------
// Validation — cheap structural invariants of a well-formed MIR function
// ---------------------------------------------------------------------------
//
// This is an *internal* sanity check (an ICE detector), not a user diagnostic:
// if it fails, the lowering produced malformed MIR — a compiler bug, not a
// program error. The HIR→MIR lowering (3.2) and every later MIR pass run it on
// their output so a bad transformation is caught at its source rather than
// surfacing as mysterious LLVM/codegen breakage.

impl MirProgram {
    /// Validate every function; returns all problems found (empty ⇒ valid).
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        for f in &self.fns {
            if let Err(mut e) = f.validate() {
                for m in &mut e {
                    *m = format!("fn {}: {}", f.name, m);
                }
                errs.append(&mut e);
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

impl MirFn {
    /// Check the structural invariants: an entry block exists, `_0` is the return
    /// slot and `_1..=arg_count` are arguments, and every [`Local`]/[`BlockId`]
    /// referenced anywhere is in range. Returns the list of violations.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let nlocals = self.locals.len() as u32;
        let nblocks = self.blocks.len() as u32;

        if self.blocks.is_empty() {
            errs.push("no basic blocks (missing entry bb0)".to_string());
        }
        if self.locals.is_empty() {
            errs.push("no locals (missing return slot _0)".to_string());
        } else if self.locals[0].kind != LocalKind::Return {
            errs.push("_0 must be the return slot".to_string());
        }
        if self.arg_count as u32 + 1 > nlocals {
            errs.push(format!(
                "arg_count {} but only {} locals",
                self.arg_count, nlocals
            ));
        } else {
            for i in 1..=self.arg_count {
                if self.locals[i].kind != LocalKind::Arg {
                    errs.push(format!("_{} should be an Arg local", i));
                }
            }
        }

        for (bi, b) in self.blocks.iter().enumerate() {
            let ctx = format!("bb{}", bi);
            for s in &b.stmts {
                match s {
                    Statement::Assign(p, rv) => {
                        v_place(p, nlocals, &ctx, &mut errs);
                        v_rvalue(rv, nlocals, &ctx, &mut errs);
                    }
                    Statement::Drop(p) => v_place(p, nlocals, &ctx, &mut errs),
                    Statement::StorageLive(l) | Statement::StorageDead(l) => {
                        v_local(*l, nlocals, &ctx, &mut errs)
                    }
                    Statement::Nop => {}
                }
            }
            match &b.term {
                Terminator::Goto(t) => v_block(*t, nblocks, &ctx, &mut errs),
                Terminator::Return | Terminator::Unreachable => {}
                Terminator::If {
                    cond,
                    then_blk,
                    else_blk,
                } => {
                    v_operand(cond, nlocals, &ctx, &mut errs);
                    v_block(*then_blk, nblocks, &ctx, &mut errs);
                    v_block(*else_blk, nblocks, &ctx, &mut errs);
                }
                Terminator::Switch {
                    discr,
                    targets,
                    default,
                } => {
                    v_operand(discr, nlocals, &ctx, &mut errs);
                    for (_, t) in targets {
                        v_block(*t, nblocks, &ctx, &mut errs);
                    }
                    v_block(*default, nblocks, &ctx, &mut errs);
                }
                Terminator::Call { func, args, dest } => {
                    v_operand(func, nlocals, &ctx, &mut errs);
                    for a in args {
                        v_operand(a, nlocals, &ctx, &mut errs);
                    }
                    if let Some((p, t)) = dest {
                        v_place(p, nlocals, &ctx, &mut errs);
                        v_block(*t, nblocks, &ctx, &mut errs);
                    }
                }
            }
        }

        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

fn v_local(l: Local, n: u32, ctx: &str, errs: &mut Vec<String>) {
    if l.0 >= n {
        errs.push(format!("{}: local _{} out of range (have {})", ctx, l.0, n));
    }
}

fn v_block(b: BlockId, n: u32, ctx: &str, errs: &mut Vec<String>) {
    if b.0 >= n {
        errs.push(format!(
            "{}: target bb{} out of range (have {})",
            ctx, b.0, n
        ));
    }
}

fn v_place(p: &Place, n: u32, ctx: &str, errs: &mut Vec<String>) {
    v_local(p.local, n, ctx, errs);
    for proj in &p.proj {
        if let Projection::Index(l) = proj {
            v_local(*l, n, ctx, errs);
        }
    }
}

fn v_operand(o: &Operand, n: u32, ctx: &str, errs: &mut Vec<String>) {
    match o {
        Operand::Copy(p) | Operand::Move(p) => v_place(p, n, ctx, errs),
        Operand::Const(_) => {}
    }
}

fn v_rvalue(rv: &Rvalue, n: u32, ctx: &str, errs: &mut Vec<String>) {
    match rv {
        Rvalue::Use(o) | Rvalue::Unary(_, o) | Rvalue::Cast(o, _) => v_operand(o, n, ctx, errs),
        Rvalue::Binary(_, a, b) => {
            v_operand(a, n, ctx, errs);
            v_operand(b, n, ctx, errs);
        }
        Rvalue::Ref(p) | Rvalue::Discriminant(p) => v_place(p, n, ctx, errs),
        Rvalue::Aggregate(_, fields) => {
            for f in fields {
                v_operand(f, n, ctx, errs);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pretty-printer (Rust-MIR-flavoured)
// ---------------------------------------------------------------------------

impl MirProgram {
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for f in &self.fns {
            out.push_str(&f.dump());
            out.push('\n');
        }
        out
    }
}

impl MirFn {
    pub fn dump(&self) -> String {
        let mut out = String::new();
        let recv = match &self.owner {
            Some(o) => format!("{}::", o),
            None => String::new(),
        };
        // Signature: args are _1..=arg_count.
        let args: Vec<String> = (1..=self.arg_count)
            .map(|i| format!("_{}: {}", i, display_ty(&self.locals[i].ty)))
            .collect();
        out.push_str(&format!(
            "fn {}{}({}) -> {} {{\n",
            recv,
            self.name,
            args.join(", "),
            display_ty(&self.locals[0].ty)
        ));
        // Local declarations.
        for (i, l) in self.locals.iter().enumerate() {
            let tag = match l.kind {
                LocalKind::Return => "ret",
                LocalKind::Arg => "arg",
                LocalKind::User => "let",
                LocalKind::Temp => "tmp",
            };
            let nm = l
                .name
                .as_deref()
                .map(|n| format!("  // {}", n))
                .unwrap_or_default();
            out.push_str(&format!(
                "    let _{}: {}; [{}]{}\n",
                i,
                display_ty(&l.ty),
                tag,
                nm
            ));
        }
        out.push('\n');
        for (i, b) in self.blocks.iter().enumerate() {
            out.push_str(&format!("    bb{}: {{\n", i));
            for s in &b.stmts {
                out.push_str(&format!("        {};\n", fmt_stmt(s)));
            }
            out.push_str(&format!("        {};\n", fmt_term(&b.term)));
            out.push_str("    }\n");
        }
        out.push_str("}\n");
        out
    }
}

fn fmt_stmt(s: &Statement) -> String {
    match s {
        Statement::Assign(p, rv) => format!("{} = {}", fmt_place(p), fmt_rvalue(rv)),
        Statement::Drop(p) => format!("drop({})", fmt_place(p)),
        Statement::StorageLive(l) => format!("StorageLive(_{})", l.0),
        Statement::StorageDead(l) => format!("StorageDead(_{})", l.0),
        Statement::Nop => "nop".to_string(),
    }
}

fn fmt_term(t: &Terminator) -> String {
    match t {
        Terminator::Goto(b) => format!("goto -> bb{}", b.0),
        Terminator::Return => "return".to_string(),
        Terminator::If {
            cond,
            then_blk,
            else_blk,
        } => format!(
            "if {} -> [true: bb{}, false: bb{}]",
            fmt_operand(cond),
            then_blk.0,
            else_blk.0
        ),
        Terminator::Switch {
            discr,
            targets,
            default,
        } => {
            let arms: Vec<String> = targets
                .iter()
                .map(|(v, b)| format!("{}: bb{}", v, b.0))
                .collect();
            format!(
                "switch {} -> [{}, otherwise: bb{}]",
                fmt_operand(discr),
                arms.join(", "),
                default.0
            )
        }
        Terminator::Call { func, args, dest } => {
            let a: Vec<String> = args.iter().map(fmt_operand).collect();
            // A direct call to a named function reads better without the `const`
            // prefix the generic operand printer would add.
            let callee = match func {
                Operand::Const(Const::Fn(name)) => name.clone(),
                other => fmt_operand(other),
            };
            match dest {
                Some((p, b)) => format!(
                    "{} = call {}({}) -> bb{}",
                    fmt_place(p),
                    callee,
                    a.join(", "),
                    b.0
                ),
                None => format!("call {}({}) -> unreachable", callee, a.join(", ")),
            }
        }
        Terminator::Unreachable => "unreachable".to_string(),
    }
}

fn fmt_place(p: &Place) -> String {
    let mut s = format!("_{}", p.local.0);
    for proj in &p.proj {
        match proj {
            Projection::Field(i) => s = format!("{}.{}", s, i),
            Projection::Index(l) => s = format!("{}[_{}]", s, l.0),
            Projection::Deref => s = format!("(*{})", s),
            Projection::Downcast(v) => s = format!("({} as variant#{})", s, v),
        }
    }
    s
}

fn fmt_operand(o: &Operand) -> String {
    match o {
        Operand::Copy(p) => format!("copy {}", fmt_place(p)),
        Operand::Move(p) => format!("move {}", fmt_place(p)),
        Operand::Const(c) => format!("const {}", fmt_const(c)),
    }
}

fn fmt_const(c: &Const) -> String {
    match c {
        Const::Int(v, ty) => format!("{}_{}", v, display_ty(ty)),
        Const::Float(v) => format!("{}", v),
        Const::Bool(b) => format!("{}", b),
        Const::Char(c) => format!("{:?}", c),
        Const::Str(s) => format!("{:?}", s),
        Const::Nil => "nil".to_string(),
        Const::Unit => "()".to_string(),
        Const::Fn(name) => name.clone(),
    }
}

fn fmt_rvalue(rv: &Rvalue) -> String {
    match rv {
        Rvalue::Use(o) => fmt_operand(o),
        Rvalue::Binary(op, a, b) => {
            format!("{:?}({}, {})", op, fmt_operand(a), fmt_operand(b))
        }
        Rvalue::Unary(op, a) => format!("{:?}({})", op, fmt_operand(a)),
        Rvalue::Cast(a, ty) => format!("{} as {}", fmt_operand(a), display_ty(ty)),
        Rvalue::Ref(p) => format!("&{}", fmt_place(p)),
        Rvalue::Discriminant(p) => format!("discriminant({})", fmt_place(p)),
        Rvalue::Aggregate(k, fields) => {
            let fs: Vec<String> = fields.iter().map(fmt_operand).collect();
            let head = match k {
                AggregateKind::Tuple => "tuple".to_string(),
                AggregateKind::Array => "array".to_string(),
                AggregateKind::Struct(n) => n.clone(),
                AggregateKind::Variant(n, v) => format!("{}::variant#{}", n, v),
                AggregateKind::Closure(name) => format!("closure {}", name),
            };
            format!("{}({})", head, fs.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — build MIR by hand and check the model + printer (3.1 has no lowering)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::IntKind;

    /// Build `fn add(a: i32, b: i32) -> i32 { a + b }` by hand:
    ///   _0 = return, _1 = a, _2 = b, _3 = temp
    ///   bb0: _3 = Add(copy _1, copy _2); _0 = move _3; return;
    fn add_fn() -> MirFn {
        let i32t = Ty::Int(IntKind::I32);
        let mut b = MirBuilder::new(
            "add",
            None,
            i32t.clone(),
            vec![
                (i32t.clone(), Some("a".into())),
                (i32t.clone(), Some("b".into())),
            ],
        );
        let tmp = b.new_temp(i32t.clone());
        let bb0 = b.new_block();
        b.push_stmt(
            bb0,
            Statement::Assign(
                Place::local(tmp),
                Rvalue::Binary(
                    BinOp::Add,
                    Operand::Copy(Place::local(Local(1))),
                    Operand::Copy(Place::local(Local(2))),
                ),
            ),
        );
        b.push_stmt(
            bb0,
            Statement::Assign(
                Place::local(MirFn::return_local()),
                Rvalue::Use(Operand::Move(Place::local(tmp))),
            ),
        );
        b.set_term(bb0, Terminator::Return);
        b.finish()
    }

    #[test]
    fn builder_numbers_locals_return_then_args_then_temps() {
        let f = add_fn();
        assert_eq!(f.arg_count, 2);
        assert_eq!(f.locals[0].kind, LocalKind::Return);
        assert_eq!(f.locals[1].kind, LocalKind::Arg);
        assert_eq!(f.locals[2].kind, LocalKind::Arg);
        assert_eq!(f.locals[3].kind, LocalKind::Temp);
        assert_eq!(MirFn::return_local(), Local(0));
        assert_eq!(MirFn::entry(), BlockId(0));
    }

    #[test]
    fn every_block_ends_in_a_terminator() {
        let f = add_fn();
        assert!(!f.blocks.is_empty(), "entry block must exist");
        for b in &f.blocks {
            // A terminator is non-optional in the type, so this is structural:
            // simply confirm it is not the placeholder left by `new_block`.
            assert!(
                !matches!(b.term, Terminator::Unreachable),
                "block terminator should have been set"
            );
        }
    }

    #[test]
    fn printer_renders_signature_locals_and_block() {
        let prog = MirProgram {
            fns: vec![add_fn()],
        };
        let dump = prog.dump();
        assert!(dump.contains("fn add(_1: i32, _2: i32) -> i32"), "{}", dump);
        assert!(dump.contains("let _0: i32; [ret]"), "{}", dump);
        assert!(dump.contains("let _3: i32; [tmp]"), "{}", dump);
        assert!(dump.contains("bb0: {"), "{}", dump);
        assert!(dump.contains("_3 = Add(copy _1, copy _2)"), "{}", dump);
        assert!(dump.contains("_0 = move _3"), "{}", dump);
        assert!(dump.contains("return"), "{}", dump);
    }

    #[test]
    fn places_operands_and_drop_render() {
        // _1.0[_2] , *_1 , drop, switch, call — exercise the printer corners.
        let p = Place {
            local: Local(1),
            proj: vec![Projection::Field(0), Projection::Index(Local(2))],
        };
        assert_eq!(fmt_place(&p), "_1.0[_2]");
        let d = Place {
            local: Local(1),
            proj: vec![Projection::Deref],
        };
        assert_eq!(fmt_place(&d), "(*_1)");
        assert_eq!(
            fmt_stmt(&Statement::Drop(Place::local(Local(3)))),
            "drop(_3)"
        );

        let sw = Terminator::Switch {
            discr: Operand::Copy(Place::local(Local(1))),
            targets: vec![(0, BlockId(1)), (1, BlockId(2))],
            default: BlockId(3),
        };
        assert_eq!(
            fmt_term(&sw),
            "switch copy _1 -> [0: bb1, 1: bb2, otherwise: bb3]"
        );

        let call = Terminator::Call {
            func: Operand::Const(Const::Fn("fib".into())),
            args: vec![Operand::Move(Place::local(Local(2)))],
            dest: Some((Place::local(Local(0)), BlockId(4))),
        };
        assert_eq!(fmt_term(&call), "_0 = call fib(move _2) -> bb4");
    }

    #[test]
    fn validate_accepts_a_well_formed_function() {
        assert!(add_fn().validate().is_ok());
        assert!(
            MirProgram {
                fns: vec![add_fn()]
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validate_flags_an_out_of_range_local() {
        let mut f = add_fn();
        // Reference a local that does not exist in bb0's first statement.
        f.blocks[0].stmts[0] = Statement::Assign(
            Place::local(Local(99)),
            Rvalue::Use(Operand::Const(Const::Unit)),
        );
        let errs = f.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("_99 out of range")),
            "{:?}",
            errs
        );
    }

    #[test]
    fn validate_flags_an_out_of_range_block_target() {
        let mut f = add_fn();
        f.blocks[0].term = Terminator::Goto(BlockId(42));
        let errs = f.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("bb42 out of range")),
            "{:?}",
            errs
        );
    }

    #[test]
    fn aggregate_and_variant_rvalues_render() {
        let agg = Rvalue::Aggregate(
            AggregateKind::Variant("Option".into(), 0),
            vec![Operand::Copy(Place::local(Local(1)))],
        );
        assert_eq!(fmt_rvalue(&agg), "Option::variant#0(copy _1)");
        let tup = Rvalue::Aggregate(
            AggregateKind::Tuple,
            vec![
                Operand::Const(Const::Int(1, Ty::Int(IntKind::I32))),
                Operand::Const(Const::Bool(true)),
            ],
        );
        assert_eq!(fmt_rvalue(&tup), "tuple(const 1_i32, const true)");
        // A closure value: `{fn ptr, env}` rendered with the lifted symbol.
        let clo = Rvalue::Aggregate(
            AggregateKind::Closure("f::{closure#0}".into()),
            vec![Operand::Copy(Place::local(Local(1)))],
        );
        assert_eq!(fmt_rvalue(&clo), "closure f::{closure#0}(copy _1)");
    }

    #[test]
    fn discriminant_rvalue_renders_and_validates() {
        // The match-tree read primitive (3.3): `discriminant(place)`.
        let downcast = Place {
            local: Local(1),
            proj: vec![Projection::Downcast(1), Projection::Field(0)],
        };
        assert_eq!(fmt_place(&downcast), "(_1 as variant#1).0");
        let d = Rvalue::Discriminant(Place::local(Local(1)));
        assert_eq!(fmt_rvalue(&d), "discriminant(_1)");

        // An out-of-range place inside a Discriminant is caught by validate.
        let mut f = add_fn();
        f.blocks[0].stmts[0] = Statement::Assign(
            Place::local(Local(3)),
            Rvalue::Discriminant(Place::local(Local(99))),
        );
        let errs = f.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("_99 out of range")),
            "{:?}",
            errs
        );
    }
}

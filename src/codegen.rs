//! LLVM back-end (Phase 5) — MIR → LLVM IR → object → linked executable.
//!
//! This is intentionally a *thin* translation layer: all the hard lowerings
//! already happened in MIR (Phase 3), so codegen stays mechanical. Phase 5.1 is
//! the **scaffold**: it wires `inkwell`/LLVM 18 into the build and proves the
//! whole tail of the pipeline end-to-end — emit a valid LLVM module, write a
//! native object with the target machine, and link it against the `la3_runtime`
//! static library into a runnable executable. It does **not** translate the
//! user program yet (functions, control flow, etc. land in 5.2+), so the module
//! it emits is the minimal one that still exercises object emission *and*
//! runtime linkage: a `main` that calls the runtime's `la3_runtime_version()`
//! and returns it as the process exit code.
//!
//! LLVM is reached through `inkwell` with the `llvm18-1-prefer-dynamic` feature
//! (see Cargo.toml); the install lives at `/usr/lib/llvm-18` via
//! `LLVM_SYS_181_PREFIX` (.cargo/config.toml).

// Like `ast.rs`/`mir.rs`: the emit/link API is exercised by this module's tests
// now and wired into the `build` command across 5.2–5.5 as real MIR→IR
// translation lands.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use inkwell::OptimizationLevel;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};

use std::collections::HashMap;

use inkwell::builder::Builder;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType, StructType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

#[allow(unused_imports)]
pub(super) use self::support::*;
#[allow(unused_imports)]
pub(super) use self::types::*;
use crate::ast::{BinOp, UnOp};
use crate::mir::{
    AggregateKind, BasicBlock, Const, MirFn, MirProgram, Operand, Place, Projection, Rvalue,
    Statement, Terminator,
};
use crate::ty::{FloatKind, IntKind, Ty};
use crate::typeck::LayoutOracle;

/// The exit code the runtime's smoke symbol returns (`la3_runtime_version`), so
/// the linked binary's exit status proves the runtime was linked and called.
pub const RUNTIME_VERSION: i32 = 1;

/// `str` is the runtime's owned `La3Str { ptr, len, cap }` — three machine words
/// (24 bytes, align 8) held **by value**, matching the realized runtime (Phase
/// 4.1: "the codegen moves by copying the three words"). Phase 6.1 models it
/// codegen-locally; the `LayoutOracle` still reports the heap types as a single
/// pointer (a placeholder), which is fine here because the milestone never embeds
/// a `str` in an aggregate (such functions stay skipped). Reconciling the oracle
/// (str = 24, and `List`/`Map`/`Set` to their real sizes) so heap values can live
/// inside aggregates is Phase 6.2.
const STR_SIZE: u64 = 24;
const STR_ALIGN: u64 = 8;

/// Per-function translation state.
struct FnGen<'a, 'ctx> {
    ctx: &'ctx Context,
    module: &'a Module<'ctx>,
    oracle: &'a LayoutOracle,
    builder: Builder<'ctx>,
    decls: &'a HashMap<String, FunctionValue<'ctx>>,
    /// Each function symbol → its parameter types (to type call arguments).
    sigs: &'a HashMap<String, Vec<Ty>>,
    f: &'a MirFn,
    /// Per-local types with `Unknown` temporaries resolved (see
    /// [`infer_local_types`]).
    local_types: Vec<Ty>,
    fval: FunctionValue<'ctx>,
    /// One `alloca` per local (`None` for unit locals, which hold no value). A
    /// scalar local's slot points at its scalar type; an aggregate's at its
    /// `[size x i8]` byte storage.
    slots: Vec<Option<PointerValue<'ctx>>>,
    /// One LLVM block per MIR block.
    blocks: Vec<inkwell::basic_block::BasicBlock<'ctx>>,
}

mod driver;
mod fngen;
mod module;
mod runtime;
mod support;
#[cfg(test)]
mod tests;
mod types;
mod value;

#[allow(unused_imports)]
pub use driver::{
    build_executable_module, compile_executable, emit_ir, emit_object, link_executable,
};
#[allow(unused_imports)]
pub use module::build_program_module;

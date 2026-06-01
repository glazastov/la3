# La3 → LLVM Compiler: Work Plan

> Living document. **Always**, at the start of a session: read this file, find
> which phase we're in, what's left inside it (the checkboxes below), and only
> then start working. When you finish a subpart: `build` + `test` + `verify`,
> tick the checkbox, and **stop for review** before moving on.

## Current state (baseline)

A complete tree-walking interpreter:

```
lexer.rs → parser.rs → ast.rs → checker.rs (names) → typeck.rs (types, light) → interp.rs (execution)
```

- Complete, faithful AST ([src/ast.rs](src/ast.rs)) — reusable.
- A semantic `enum Ty` already exists in [src/typeck.rs](src/typeck.rs#L61) with **light**
  inference/unification.
- Dynamically-typed interpreter (`enum Value`, [src/interp.rs](src/interp.rs#L27)), heap via
  `Rc`/`RefCell`, stdlib as Rust builtins.
- The spec ([laila-lang-reference.md](laila-lang-reference.md)) says La3 "is not intended to be
  compiled": several points are **loosely specified** (GC vs ownership, `any`, lifetimes) →
  they require decisions from us.

## Design decisions (v1)

| Topic          | v1 decision                                                         | Why                                          |
| -------------- | ------------------------------------------------------------------- | -------------------------------------------- |
| Backend        | LLVM via `inkwell` (LLVM 18)                                        | Industry standard; LLVM 18 already installed |
| Memory         | **Ownership** (move semantics + full borrow checker, deterministic drop) | User decision 2026-06-01; matches reference Section 11. _Supersedes the earlier ARC v1 plan._ |
| Generics       | Monomorphization                                                    | Required for static layout                   |
| IR             | AST → HIR (typed, desugared) → MIR (mono, layout, RC, match) → LLVM | The layer that makes the rest viable         |
| Test oracle    | The interpreter stays alive for _differential testing_              | Cheap confidence per phase                   |
| Initial target | `x86_64-unknown-linux-gnu` native                                   | Reduces scope                                |
| Toolchain      | rustup `stable` (≥1.94), edition 2024                               | pinned by `rust-toolchain.toml`              |

### v1 subset (explicit cuts for the first binary)

Deferred to late/future phases: `async`/`await`/`all`/`race`, `try`/`catch` (unwinding),
real `net`/`crypto`. `Result`/`Option`/`?` land early (just enums). **The borrow checker is
now in scope** (no longer deferred) — see the new Phase 1.6.

---

## Status convention

- `[ ]` not started · `[~]` in progress · `[x]` done and verified (build+test+verify)
- A phase is only "done" when **all** its subparts are `[x]` and the user has approved.

---

## Phase 0 — Foundations & decisions · **STATUS: [x] done (awaiting review)**

- [x] 0.1 `rust-toolchain.toml` pinning stable; `Cargo.toml` → edition 2024; clean `cargo build`/`test`
- [x] 0.2 `COMPILER_PLAN.md` (this file) + `CLAUDE.md` with rules
- [x] 0.3 `la3 build <file.la3>` subcommand (stub: parse+check, exit 3 "codegen pending")
- [x] 0.4 `runtime/` crate (workspace member) — `RcHeader`/`Tag` + `la3_rc_inc`/`la3_rc_dec` skeleton
- [x] 0.5 Differential test harness ([tests/differential.rs](tests/differential.rs)) — interp × future binary, auto-skips while codegen pending
- [x] 0.6 Document LLVM env/ABI (`LLVM_SYS_181_PREFIX=/usr/lib/llvm-18`) in `CLAUDE.md`

## Phase 1 — Sound type checker · STATUS: [x] done (awaiting review)

Prerequisite for everything. The light `typeck` becomes the source of truth.

- [x] 1.1 Annotate **every AST node** with a concrete `Ty` — `NodeId` on `Expr` ([ast.rs](src/ast.rs)), `Program::assign_ids`, `typeck::check_types` → `TypeTable`, debug `la3 types`
- [x] 1.2 Full field & method resolution — `builtin_method_sig` now returns `Option` (None = no such method); unresolved field on a known struct/tuple or method on a known type (`resolves_methods`) is an error; lenient on `Unknown`/generics/pointers
- [x] 1.3 _Layout_ computation — C-style structs/tuples/`[T;N]`, tagged-union enums (incl. `Option`/`Result`), heap handles pointer-sized; `VariantKind` now carries payload types; debug `la3 layout`
- [x] 1.4 Exact `as` semantics — type checker now validates cast legality (`TypeChecker::check_cast`): `as` converts numeric↔numeric and integer↔`char` only, rejecting e.g. `str as i32`/`bool as f64` (lenient on `Unknown`/generic/pointer/ref). Runtime exactness confirmed/fixed: `/` truncates toward zero, `%` takes the left sign, `**`→`f64`, `as` truncates+sign via `mask_int`/`mask_uint` and float→int `trunc()`; **fixed `idiv`** (floor `//`) which used `div_euclid` and was wrong for a negative divisor (`idiv(7,-2)` now `-4`). Battery: `tests/casts.rs`
- [x] 1.5 Sound inference — unconstrained literals now default to `i32`/`f64` (`relations::default_ty`, applied to the finished `TypeTable` in `check_types`); contextual **pinning** (`TypeChecker::pin_literals`) records annotated literals at their target width (`let x: u8 = 42`, array elements, call arguments) instead of the default. No implicit widening/narrowing (already enforced by `assignable`) confirmed with real type errors. Battery: `tests/inference.rs`

## Phase 1.6 — Ownership & borrow checker · STATUS: [ ] ← user decision 2026-06-01 (replaces ARC)

Full Rust-style ownership, checked statically. The interpreter (`Rc`-based) stays the
oracle; ownership is a compile-time analysis the back-end relies on for deterministic
drop. Seeds already present: `check_borrow_conflicts` (aliasing-xor-mutability on call
args), `borrow_root`, `unsafe_depth`. Likely subparts (to refine next session):

- [ ] 1.6.1 Move semantics: track moved-out bindings; **use-after-move** is an error; `move` closures take ownership of captures
- [ ] 1.6.2 Borrows: `&T`/`&mut T` exclusivity beyond single calls (a live `&mut` forbids other borrows); reborrow rules
- [ ] 1.6.3 Lifetimes: a reference may not outlive its referent (reject returning/storing a borrow of a local)
- [ ] 1.6.4 Drop & ownership-aware codegen contract (deterministic destruction order; what MIR must carry)

## Phase 2 — HIR + desugaring · STATUS: [ ]

- [ ] 2.1 Define `hir.rs` (typed AST, no sugar)
- [ ] 2.2 Lowering: f-strings → format calls; `?.`/`??` → nil match
- [ ] 2.3 Lowering: `if let`/`while let`/`for..in` → match/iterator; compound `+=` etc.
- [ ] 2.4 Explicit closures and captures in HIR

## Phase 3 — Runtime library · STATUS: [ ]

- [ ] 3.1 `str` layout & ABI (UTF-8), RC (`rc_inc`/`rc_dec`/drop)
- [ ] 3.2 `List<T>`, `Map`, `Set` in the runtime
- [ ] 3.3 f-string formatting with specs (`:02x`, `:.1f`, `:>20`)
- [ ] 3.4 `extern "C"` stdlib: `io`, `fs`, `os`, `math`, `bytes`, `json` (subset)

## Phase 4 — Core codegen · STATUS: [ ] ← first binary that runs

- [ ] 4.1 Add `inkwell`; emit empty LLVM module + link runtime
- [ ] 4.2 Functions, params, return, scalars, arithmetic (exact semantics)
- [ ] 4.3 Control flow: `if`/`loop`/`while`/`break with value`/`return`
- [ ] 4.4 Structs by value; enums as tagged unions
- [ ] 4.5 Compile `match` → decision tree (guards, ranges, `@`, or-patterns)
- [ ] 4.6 **Milestone**: `fizzbuzz.la3`, `fib.la3`, `shapes.la3` compile and match the interpreter

## Phase 5 — Memory: heap, refs, pointers · STATUS: [ ]

- [ ] 5.1 `List`/`Map`/`Set`/`str` via runtime + RC insertion in MIR
- [ ] 5.2 `&T`/`&mut T` (safe refs); `*r`/`*r = v`
- [ ] 5.3 `*T`/`*mut T`, `&raw`, `sizeof(T)`-scaled arithmetic, `unsafe`, `alloc`/`dealloc`
- [ ] 5.4 **Milestone**: `collections`, `memory`, `tls_record`, `word_count`

## Phase 6 — Generics & interfaces · STATUS: [ ]

- [ ] 6.1 Monomorphization (collect concrete instances, emit copies)
- [ ] 6.2 Interfaces: static dispatch via bounds
- [ ] 6.3 Dynamic dispatch via vtables when needed

## Phase 7 — Closures · STATUS: [ ]

- [ ] 7.1 Closures = {fn ptr, heap env}; `move` vs by-ref capture
- [ ] 7.2 Higher-order methods (`map`/`filter`/`reduce`/`sort_by`/`group_by`)

## Phase 8 — Errors · STATUS: [ ]

- [ ] 8.1 `Result`/`Option`/`?` (early return over enums)
- [ ] 8.2 `try`/`catch`/`finally` with unwinding (landing pads + personality) — may defer

## Phase 9 — Concurrency (most expensive; last) · STATUS: [ ]

- [ ] 9.1 `spawn`/`join`/channels over OS threads
- [ ] 9.2 `async`/`await`/`all`/`race` via state machines + executor

## Phase 10 — Driver & quality · STATUS: [ ]

- [ ] 10.1 Pipeline: object → link runtime → executable; `-O` flags, target
- [ ] 10.2 Conformance: interp×compiled differential over all `examples/` + `tests/`
- [ ] 10.3 Golden IR tests; (future) DWARF debug info

---

## Progress log

- 2026-05-31 — Plan created. Toolchain: rustup stable 1.94 (system 1.75 ignored). LLVM 18 at `/usr/lib/llvm-18`. Starting Phase 0.
- 2026-05-31 — **Phase 0 complete.** Removed the conflicting Ubuntu system Rust 1.75 (`apt`), so plain `cargo` is now rustup `stable` 1.96 (edition 2024). Added `runtime/` crate, `la3 build` stub, and the differential harness (13 examples, all skipped pending codegen). `cargo test --workspace`: 33 + 2 pass. Awaiting review before Phase 1.
- 2026-05-31 — **Phase 1.1 done.** Added `NodeId` to `Expr`, numbered by `Program::assign_ids` (called in `parser::parse`). Type checker now records a concrete `Ty` per node into a `TypeTable` (`typeck::check_types`); new `la3 types` command dumps it. Tests: 53 pass (added `types_command_annotates_all_examples`).
- 2026-05-31 — **Phase 1.2 done.** Field/method resolution now errors instead of silently yielding `Unknown`: unknown struct field, bad tuple index, and unknown method on a fully-modeled receiver (`resolves_methods`) are reported with spans. `builtin_method_sig` returns `Option`. Stays lenient on `Unknown`/`Param`/pointers/refs to avoid false positives. Tests: 60 pass (+7 `p12_*`).
- 2026-05-31 — **Phase 1.3 done.** By-value layout (`size_align`, `aggregate_sa`, `enum_layout_info`): C-style aggregates, tagged-union enums (incl. built-in `Option`/`Result`), fixed arrays, heap handles pointer-sized, slices as fat pointers. Fixed a real gap — the parser discarded enum-variant payload types, so `VariantKind` now stores `TypeExpr`s. New `la3 layout` command + 9-test battery (`tests/layout.rs`). 69 tests pass.
- 2026-06-01 — **Refactor (modularization).** Split the two remaining monoliths into focused submodules, mirroring the earlier `parser/`+`typeck/` split: `interp.rs` 2984→854 lines over `src/interp/{stmts,exprs,matching,loops,concurrency,calls,convert,builtins}.rs`; `typeck.rs` 1862→351 lines over `src/typeck/{collect,driver,stmts,infer,calls,control}.rs` (alongside the existing `builtins`/`layout`/`relations`). Pure reorganization (`use super::*;`, methods `pub(super)`), no behavior change. 69 tests still pass.
- 2026-06-01 — **Phase 1.4 done.** `as` cast legality enforced statically (`TypeChecker::check_cast`): numeric↔numeric and integer↔`char` only; `str as i32`/`bool as f64` are now type errors (lenient on `Unknown`/generic/pointer/ref). Confirmed runtime exactness for `/` (trunc toward zero), `%` (left sign), `**`→`f64`, and `as` truncation/sign; **fixed `idiv`** floor division, which used `div_euclid` and rounded wrong for a negative divisor (`idiv(7,-2)`: −3 → −4). New battery `tests/casts.rs` (10 tests). 79 tests pass.
- 2026-06-01 — **Phase 1 complete.** **1.5 done:** literal defaulting (`relations::default_ty` over the finished table) + contextual pinning (`TypeChecker::pin_literals` at `let`-with-annotation, `return`, and call args) — `la3 types` is now fully concrete, no `{integer}`/`{float}` left; no-implicit-widening confirmed with real errors. New battery `tests/inference.rs` (9 tests). 88 tests pass. **Decision:** user chose full **Ownership** (move + borrow checker, deterministic drop) over the earlier ARC plan — decision table + cuts updated, new **Phase 1.6** added. Awaiting review before Phase 1.6.

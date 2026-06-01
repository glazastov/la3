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

| Topic          | v1 decision                                                                                          | Why                                                                                           |
| -------------- | ---------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| Backend        | LLVM via `inkwell` (LLVM 18)                                                                         | Industry standard; LLVM 18 already installed                                                  |
| Memory         | **Ownership** (move semantics + full borrow checker, deterministic drop)                             | User decision 2026-06-01; matches reference Section 11. _Supersedes the earlier ARC v1 plan._ |
| Generics       | Monomorphization                                                                                     | Required for static layout                                                                    |
| IR             | AST → HIR (typed, desugared) → MIR (mono, layout, drop insertion, match trees, closure conv.) → LLVM | MIR is its own phase (3) — see the back-end layering note                                     |
| Test oracle    | The interpreter stays alive for _differential testing_                                               | Cheap confidence per phase                                                                    |
| Initial target | `x86_64-unknown-linux-gnu` native                                                                    | Reduces scope                                                                                 |
| Toolchain      | rustup `stable` (≥1.94), edition 2024                                                                | pinned by `rust-toolchain.toml`                                                               |

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

## Phase 1.6 — Ownership & borrow checker · STATUS: [~] in progress ← user decision 2026-06-01 (replaces ARC)

Full Rust-style ownership, checked statically by a new `borrowck` pass that runs from
`checker::check` after a clean type check (so it has a reliable `TypeTable`). The
interpreter (`Rc`-based) stays the oracle; ownership is a compile-time analysis the
back-end relies on for deterministic drop. Seeds already present: `check_borrow_conflicts`
(aliasing-xor-mutability on call args), `borrow_root`, `unsafe_depth`.

**Design decisions (recorded 2026-06-01):**

- _Copy vs move_ (`TypeTable::is_copy`): scalars (`bool`/ints/floats/`char`/`()`), `nil`,
  references `&T`, raw pointers `*T`, slices `&[T]`, ranges, and `fn` are **Copy**;
  `str`, `List`/`Map`/`Set`, tuples/arrays _of non-Copy_, structs, enums, futures, and
  unions are **move**. `Unknown`/generic `Param` are treated as Copy (lenient — never
  invent a move on a type we don't fully model).
- _What moves a value_: only **unambiguous** moves where the caller syntax alone decides
  it — `let y = x` and `x = y` (whole-binding) and (1.6.2) `move`-closure captures. A
  `&x`/`&mut x` is a borrow, not a move. **Argument/receiver moves are deferred to 1.6.2**
  because they need callee/`self` signatures (e.g. `m.get(word)` borrows `word`,
  `xs.map(..)` borrows the receiver — proven by the examples, which reuse both).
- _Flow_: analysis is flow-sensitive — straight-line threading, `if`/`match` branch
  **union** (moved in any branch ⇒ moved after, matching Rust), and a two-pass loop check
  (a value moved in one iteration and used in the next is an error). `let`/`=` re-init
  clears the moved mark.

- [x] 1.6.1 Move semantics — `borrowck` pass + `TypeTable::is_copy`; moves via `let y = x` / `x = y`; **use-after-move** is an error, flow-sensitive (branch union + two-pass loops). Argument/receiver moves and `move`-closure captures deferred to 1.6.2. Battery: `tests/ownership.rs`
- [x] 1.6.2 Argument & receiver moves — by-value param ⇒ move the bare-binding arg; `self`/`mut self` receiver ⇒ move the receiver; `&`/`&mut`/slice params and **built-ins** borrow. Needed `SelfKind` on `FnDecl` (parser records `self` vs `&self`) and a call-signature map in `borrowck`; surfaced a real ownership bug in `http_server.la3` (passed `Request` by value then read a field) — fixed it to borrow (`route(req: &Request)`), and taught the interpreter to auto-deref refs in field access. Battery extended in `tests/ownership.rs`
- [x] 1.6.3 `move`-closure captures — a `move` closure takes ownership of every non-Copy free variable it captures (`closure_free_vars` collects them, over-subtracting bound names to stay false-positive-free); a non-`move` closure borrows. Battery extended in `tests/ownership.rs`
- [ ] 1.6.4 **Borrow regions** — `&T`/`&mut T` exclusivity beyond single calls (a live `&mut` forbids other borrows; reborrows) **and** lifetimes (a reference may not outlive its referent). Combined because both need the same borrow-region/liveness analysis (when a borrow stored in a binding is live). The within-call case is already handled by `check_borrow_conflicts`.
- [ ] 1.6.5 Drop & ownership-aware codegen contract (deterministic destruction order; what MIR must carry)

> **Back-end layering (why MIR is its own phase).** The pipeline is
> `AST → [type + borrow check] → HIR → MIR → LLVM IR → object → link runtime`.
> HIR is the typed, desugared tree; **MIR** is a control-flow graph of basic
> blocks with explicit temporaries where the _hard_ lowerings live —
> monomorphization, match decision trees, closure conversion, and ownership
> lowering (drop insertion + borrow→pointer). Keeping MIR explicit stops that
> logic from leaking into the LLVM back-end, which should be a thin, mostly
> mechanical MIR→IR translation. Note the split: Phase 1.6 _checks_ ownership
> (rejects bad programs on the AST); MIR 3.5 _lowers_ it (inserts the drops the
> check proved correct).

## Phase 2 — HIR + desugaring · STATUS: [ ]

Typed tree, all sugar removed, but still tree-shaped (no CFG yet).

- [ ] 2.1 Define `hir.rs` (typed AST, no sugar)
- [ ] 2.2 Lowering: f-strings → format calls; `?.`/`??` → nil match
- [ ] 2.3 Lowering: `if let`/`while let`/`for..in` → match/iterator; compound `+=` etc.
- [ ] 2.4 Explicit closures and captures in HIR

## Phase 3 — MIR (the layer that makes the rest viable) · STATUS: [ ]

A CFG of basic blocks with explicit temporaries and typed locals. **Every hard
transformation happens here**, so Phase 5 (LLVM) stays a thin translation.

- [ ] 3.1 Define `mir.rs`: basic blocks + terminators, explicit temporaries, typed locals, explicit drop points
- [ ] 3.2 **Monomorphization** — collect concrete generic instances, emit a specialized copy per instantiation (required for static layout)
- [ ] 3.3 **Match → decision trees** (guards, ranges, `@`, or-patterns, exhaustive default)
- [ ] 3.4 **Closure conversion** — closures → `{fn ptr, captured env}`; `move` vs by-ref capture made explicit
- [ ] 3.5 **Ownership lowering** — consume the Phase 1.6 borrow-check facts: insert deterministic `drop`s at end-of-scope/last-use, lower `&T`/`&mut T` to pointers, thread moves
- [ ] 3.6 Lower HIR control flow (`if`/`loop`/`while`/`break`-with-value) into the CFG

## Phase 4 — Runtime library · STATUS: [ ]

The native runtime compiled code links against (ownership model: owned values +
`drop`, **not** ARC).

- [ ] 4.1 `str` layout & ABI (UTF-8) + `drop` glue
- [ ] 4.2 `List<T>`, `Map`, `Set` in the runtime (owned; `drop` frees)
- [ ] 4.3 f-string formatting with specs (`:02x`, `:.1f`, `:>20`)
- [ ] 4.4 `extern "C"` stdlib: `io`, `fs`, `os`, `math`, `bytes`, `json` (subset)

## Phase 5 — LLVM codegen (MIR → IR) · STATUS: [ ] ← first binary that runs

Thin, mechanical translation of MIR to LLVM IR — no language logic here.

- [ ] 5.1 Add `inkwell`; emit empty LLVM module + link runtime
- [ ] 5.2 Functions, params, return, scalars, arithmetic (exact semantics)
- [ ] 5.3 Control flow from the MIR CFG; `break with value`/`return`
- [ ] 5.4 Structs by value; enums as tagged unions; the lowered match trees
- [ ] 5.5 **Milestone**: `fizzbuzz.la3`, `fib.la3`, `shapes.la3` compile and match the interpreter

## Phase 6 — References, raw pointers, unsafe · STATUS: [ ]

Codegen for the memory features (the _checking_ is Phase 1.6; the _lowering_ is MIR 3.5).

- [ ] 6.1 `List`/`Map`/`Set`/`str` codegen against the runtime
- [ ] 6.2 `&T`/`&mut T` (safe refs); `*r`/`*r = v`
- [ ] 6.3 `*T`/`*mut T`, `&raw`, `sizeof(T)`-scaled arithmetic, `unsafe`, `alloc`/`dealloc`
- [ ] 6.4 **Milestone**: `collections`, `memory`, `tls_record`, `word_count`

## Phase 7 — Generics & interfaces · STATUS: [ ]

(Monomorphization itself is MIR 3.2; this is dispatch.)

- [ ] 7.1 Interfaces: static dispatch via bounds
- [ ] 7.2 Dynamic dispatch via vtables when needed

## Phase 8 — Closures & higher-order methods · STATUS: [ ]

(Closure _conversion_ is MIR 3.4; this is the codegen + library on top.)

- [ ] 8.1 Codegen for converted closures = `{fn ptr, heap env}`
- [ ] 8.2 Higher-order methods (`map`/`filter`/`reduce`/`sort_by`/`group_by`)

## Phase 9 — Errors · STATUS: [ ]

- [ ] 9.1 `Result`/`Option`/`?` (early return over enums)
- [ ] 9.2 `try`/`catch`/`finally` with unwinding (landing pads + personality) — may defer

## Phase 10 — Concurrency (most expensive; last) · STATUS: [ ]

- [ ] 10.1 `spawn`/`join`/channels over OS threads
- [ ] 10.2 `async`/`await`/`all`/`race` via state machines + executor

## Phase 11 — Driver & quality · STATUS: [ ]

- [ ] 11.1 Pipeline: object → link runtime → executable; `-O` flags, target
- [ ] 11.2 Conformance: interp×compiled differential over all `examples/` + `tests/`
- [ ] 11.3 Golden IR tests; (future) DWARF debug info

---

## Progress log

- 2026-05-31 — Plan created. Toolchain: rustup stable 1.94 (system 1.75 ignored). LLVM 18 at `/usr/lib/llvm-18`. Starting Phase 0.
- 2026-05-31 — **Phase 0 complete.** Removed the conflicting Ubuntu system Rust 1.75 (`apt`), so plain `cargo` is now rustup `stable` 1.96 (edition 2024). Added `runtime/` crate, `la3 build` stub, and the differential harness (13 examples, all skipped pending codegen). `cargo test --workspace`: 33 + 2 pass. Awaiting review before Phase 1.
- 2026-05-31 — **Phase 1.1 done.** Added `NodeId` to `Expr`, numbered by `Program::assign_ids` (called in `parser::parse`). Type checker now records a concrete `Ty` per node into a `TypeTable` (`typeck::check_types`); new `la3 types` command dumps it. Tests: 53 pass (added `types_command_annotates_all_examples`).
- 2026-05-31 — **Phase 1.2 done.** Field/method resolution now errors instead of silently yielding `Unknown`: unknown struct field, bad tuple index, and unknown method on a fully-modeled receiver (`resolves_methods`) are reported with spans. `builtin_method_sig` returns `Option`. Stays lenient on `Unknown`/`Param`/pointers/refs to avoid false positives. Tests: 60 pass (+7 `p12_*`).
- 2026-05-31 — **Phase 1.3 done.** By-value layout (`size_align`, `aggregate_sa`, `enum_layout_info`): C-style aggregates, tagged-union enums (incl. built-in `Option`/`Result`), fixed arrays, heap handles pointer-sized, slices as fat pointers. Fixed a real gap — the parser discarded enum-variant payload types, so `VariantKind` now stores `TypeExpr`s. New `la3 layout` command + 9-test battery (`tests/layout.rs`). 69 tests pass.
- 2026-06-01 — **Refactor (modularization).** Split the two remaining monoliths into focused submodules, mirroring the earlier `parser/`+`typeck/` split: `interp.rs` 2984→854 lines over `src/interp/{stmts,exprs,matching,loops,concurrency,calls,convert,builtins}.rs`; `typeck.rs` 1862→351 lines over `src/typeck/{collect,driver,stmts,infer,calls,control}.rs` (alongside the existing `builtins`/`layout`/`relations`). Pure reorganization (`use super::*;`, methods `pub(super)`), no behavior change. 69 tests still pass.
- 2026-06-01 — **Phase 1.4 done.** `as` cast legality enforced statically (`TypeChecker::check_cast`): numeric↔numeric and integer↔`char` only; `str as i32`/`bool as f64` are now type errors (lenient on `Unknown`/generic/pointer/ref). Confirmed runtime exactness for `/` (trunc toward zero), `%` (left sign), `**`→`f64`, and `as` truncation/sign; **fixed `idiv`** floor division, which used `div_euclid` and rounded wrong for a negative divisor (`idiv(7,-2)`: −3 → −4). New battery `tests/casts.rs` (10 tests). 79 tests pass.
- 2026-06-01 — **Roadmap restructure (explicit MIR).** The pipeline declared `AST → HIR → MIR → LLVM` but the phases jumped HIR (2) → LLVM (4), with no MIR phase — the hard lowerings (monomorphization, drop insertion, borrow lowering, match decision trees, closure conversion) had no home and risked leaking into the back-end. Inserted **Phase 3 — MIR** and renumbered: Runtime 3→4, LLVM codegen 4→5, refs/pointers 5→6, generics 6→7, closures 7→8, errors 8→9, concurrency 9→10, driver 10→11. Relocated mono (was 6.1)→3.2, match trees (was 4.5)→3.3, closure conversion (was 7.1)→3.4, and added ownership lowering 3.5. Clarified the split: 1.6 _checks_ ownership, MIR 3.5 _lowers_ it. Updated CLAUDE.md/README cross-refs (LLVM is now Phase 5) and the IR/Memory rows (RC→drop). No code behavior change.
- 2026-06-01 — **Phase 1.6.1 done.** New `borrowck` pass ([src/borrowck.rs](src/borrowck.rs)), run from `checker::check` after a clean type check (so `la3 check`/`run`/`build` all enforce it). Move semantics: `TypeTable::is_copy` classifies Copy vs move types; `let y = x` / `x = y` of a non-Copy binding moves it, and a later read is **use-after-move**. Flow-sensitive: `if`/`match` branch union + two-pass loop check; `let`/`=` re-init clears the mark. Argument/receiver moves and `move`-closure captures are deferred to 1.6.2 (proven necessary: `xs.map(..)` borrows its receiver and `m.get(k)` borrows its arg, so the examples reuse both). Zero false positives across all examples. New battery `tests/ownership.rs` (10 tests). 98 tests pass. Awaiting review before 1.6.2.
- 2026-06-01 — **Phase 1 complete.** **1.5 done:** literal defaulting (`relations::default_ty` over the finished table) + contextual pinning (`TypeChecker::pin_literals` at `let`-with-annotation, `return`, and call args) — `la3 types` is now fully concrete, no `{integer}`/`{float}` left; no-implicit-widening confirmed with real errors. New battery `tests/inference.rs` (9 tests). 88 tests pass. **Decision:** user chose full **Ownership** (move + borrow checker, deterministic drop) over the earlier ARC plan — decision table + cuts updated, new **Phase 1.6** added. Awaiting review before Phase 1.6.
- 2026-06-01 — **Phase 1.6.2 done.** Argument & receiver moves. Added `SelfKind` to `FnDecl` (the parser now records `self`/`mut self` vs `&self`/`&mut self`, which it previously discarded); `borrowck` builds a call-signature map (free fns, `(type, method)`, declared type names) and moves a bare-binding argument when its parameter is by value, and the receiver when the method takes `self`/`mut self`. `&`/`&mut`/slice params and all **built-ins** borrow. Fixed a loop-carry bug the new moves exposed (a `for s in xs` body that moves `s` must not flag the next iteration). Surfaced a genuine ownership bug in `http_server.la3` (`route(req: Request)` consumed `req`, then read `req.path`) → changed `route` to borrow `&Request`, and taught the interpreter to **auto-deref references in field access** (`r.field`). 5 new ownership tests (15 total in `tests/ownership.rs`). 103 tests pass. Awaiting review before 1.6.3.
- 2026-06-01 — **Phase 1.6.3 done.** `move`-closure captures: a `move` closure takes ownership of every non-Copy free variable it captures, so those bindings are moved once it is created (`closure_free_vars` walks the body collecting identifier refs minus parameter/`let`/pattern-bound names — over-subtracting to never invent a capture). A non-`move` closure borrows. `move`-closure capturing a `Copy` value (e.g. `i32`) leaves the original usable. 3 new tests (18 total in `tests/ownership.rs`). 106 tests pass. **Re-split:** `&mut` exclusivity merged with lifetimes into 1.6.4 (both need borrow-region/liveness analysis). Awaiting review before 1.6.4.

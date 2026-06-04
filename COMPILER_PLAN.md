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

## North Star — pillars beyond v1 (recorded 2026-06-04)

These are **durable goals**, not v1 deliverables. They are written here so every
phase keeps them in view and makes choices that don't paint us into a corner. We
are not in a hurry — they are to be done _well_.

### Pillar 1 — one language, low-level **and** high-level

La3 must be good enough to write a Raspberry Pi Pico bootloader, kernel, and
bare-metal programs **and** to write web apps and PC applications. The _front-end
already serves both_: exact-width integers, ownership + borrow checking, raw
pointers / `unsafe`, and C-style by-value layout are exactly what bare-metal
needs; nothing is wasted. The real split is the **runtime + target**, not the
language.

### Pillar 2 — the **à-la-carte dynamic stdlib** (the chosen answer to "embedded", _not_ `no_std`)

> **Full technical specification: [DYNAMIC_STDLIB.md](DYNAMIC_STDLIB.md)** — the
> authoritative design (requirements R1–R6, module/capability model, the resolver
> and its fixpoint, the two build modes, ABI, worked examples, open questions).

We explicitly **reject a `no_std` split**. Instead the standard library is a set
of **many small, fully independent modules**, and the build pulls in **only what
the program actually uses**. Precisely (user decision 2026-06-04, a core pillar):

- **Independence is the invariant.** Each stdlib module is self-contained: used
  _alone_, it must depend on **nothing** else. No hard inter-module edges.
- **Opportunistic sharing when co-present.** If a program uses `stdlibA` _and_
  `stdlibB`, and `A` could be implemented more cheaply by reusing something `B`
  already brings in, then `A` **may** fold onto `B`'s implementation — becoming
  smaller — _without_ ever making `A` depend on `B` in the alone case. Sharing is
  an optimization that only ever _removes_ duplication already present.
- **Two build modes.** A build flag (e.g. `--stdlib granular`) selects the
  independent + opportunistic-sharing + aggressive dead-code-elimination mode
  (small binaries: Pico, kernels). The **default** mode compiles the whole stdlib
  freely interdependent, tuned for full apps on PC/Web.
- **Behavioural identity.** A granular build and a monolithic build of the _same_
  program must behave identically; only size/layout differ. (This is the headline
  conformance test for the feature.)

**Cross-cutting hooks (so we don't block this later):** author the **runtime
(Phase 4)** as independent modules from the start; lean on the **MIR reachability
walk** (the call graph from `main`, available once HIR→MIR lowering lands in 3.2 —
the same walk monomorphization 7.1 reuses) as the dead-code-elimination substrate;
keep the **driver/link step (11.1)** mode-aware. The mechanism itself is **Phase 12**.

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

## Phase 1.6 — Ownership & borrow checker · STATUS: [x] done (awaiting review) ← user decision 2026-06-01 (replaces ARC)

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
- [x] 1.6.4 **Borrow regions** — a `let`-bound borrow (`let r = &x` / `&mut x`) is tracked **lexically** (live until the end of the block that declared `r`, a sound pre-NLL approximation): a live `&mut x` forbids _any_ other access to `x`; a live `&x` forbids writes to `x`, including **mutation through a method** (`SelfKind` distinguishes `&self` from `&mut self`, and `method_mutates` knows the built-in in-place mutators `push`/`pop`/`insert`/`remove`/`extend`/`clear`/…). Borrows are **field-granular** (`Place` = root + projection path): `&u.name` does not lock `u.age`; a whole-value borrow locks every field. Lifetimes: returning a borrow of a bare local/param (`return &x`) is a dangling-reference error. The within-call case stays with `check_borrow_conflicts`; temporary `&mut x` arguments don't lock.

  > **⚠️ Gaps that can only be fixed later (on the MIR), recorded 2026-06-02.** Two precision gaps are **not** solvable soundly on the AST and are deliberately deferred to a borrow-check pass over the **MIR CFG** (Phase 3) — this is exactly why real Rust borrow-checks on MIR:
  >
  > - **NLL (non-lexical lifetimes):** a borrow should end at its _last use_, not the end of its block. A textual "last use" heuristic is **unsound** under loops (`let r=&v; loop { print(r); v.push(1) }` — the back-edge makes `r` live at the push), so it needs CFG liveness. Until then the lexical approximation may _reject some safe programs_, but never accepts an unsound one.
  > - **Reborrows:** `let r2 = &*r1` must know `r2` reaches the same root as `r1` through the borrow chain; sound tracking wants the MIR's explicit places/regions.
  >
  > Two more are **intentionally conservative, matching Rust** (not bugs): **index borrows** (`&a[0]` locks the whole array — indices are dynamic; disjointness needs an explicit API like `split_at_mut`), and the **built-in mutating-method list** is hand-maintained.

- [x] 1.6.5 **Drop & ownership-aware codegen contract** — the front-end now classifies which types own heap and need a `drop` (`TypeChecker::ty_needs_drop`): heap-owning built-ins (`str`/`List`/`Map`/`Set`/future) and any aggregate transitively containing one; scalars/refs/raw pointers/slices/`fn` don't. Surfaced via `la3 layout` (`drop=yes/no` per struct/enum) and battery `tests/drops.rs`. This is the front-end half; the **contract MIR 3.5 must honour**:
  - _What:_ drop a value iff `ty_needs_drop`.
  - _When:_ each owned binding is dropped at the end of its scope (or, once NLL lands on the MIR CFG, at its last use), in **reverse declaration order**.
  - _Skip moved:_ a binding moved out (per the 1.6.1–1.6.3 move analysis) is **not** dropped — its new owner is responsible. A _conditionally_ moved binding needs a runtime **drop flag**.
  - _Partial moves:_ with the field-granular `Place` info, a value with one field moved out drops only its remaining fields.
  - _Carry:_ MIR must thread, per scope, the owned locals + their move/borrow state so 3.5 inserts `drop`s (and drop flags) at exactly the proven-safe points.

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

## Phase 2 — HIR + desugaring · STATUS: [x] done (awaiting review)

Typed tree, all sugar removed, every local resolved to a unique `BindingId`. Still
tree-shaped (no CFG — that's MIR) and still generic (monomorphization is MIR 3.2).

**Agreed shape (2026-06-02):**

- **`Ty` is shared** — extracted to `src/ty.rs` and **embedded** in every HIR node, so
  the back-end never re-infers (typeck/borrowck/hir all use the one `Ty`).
- **Bindings resolved in name resolution.** The resolver assigns a unique `BindingId`
  to each binding site (`let`, param, pattern binding, closure param) and maps every
  identifier _use_ to its binding. **Shadowing is resolved there, once** — HIR (and
  later passes) work on IDs and never reason about names/shadowing again. (The borrow
  checker could later adopt these IDs in place of its name-based tracking.)
- **`for..in` stays a typed HIR node**; the iteration _step_ is lowered per iterable
  kind in MIR (Range→counter, List→index, …) — no user-facing iterator trait in v1.
- HIR carries **types + structure only**; ownership/borrow/drop facts are re-derived on
  the MIR CFG (NLL needs the CFG anyway).

**`Ty` review (2026-06-02), decisions recorded:**

- **`str` is owned** (String-like: heap-backed, move-only, dropped), not a borrowed
  view — the reference calls it a "slice" but every use (`List<str>`, `split()→List<str>`,
  `+`, no surface lifetimes) only closes if it owns its buffer. `&[u8]` (`Slice`) is the
  borrowed counterpart. (This matches what `ty_is_copy`/`ty_needs_drop` already assume.)
- **`Ty: Eq + Hash`** derived now (no `f64` payload), so MIR 3.2 can key monomorphized
  instances by concrete type.
- _Invariant:_ `IntLit`/`FloatLit` are inference-only; after `default_ty` (1.5) a recorded
  type is concrete, so HIR/MIR never see them (lowering may `debug_assert` it).
- _Deferred:_ `Ref`/`Ptr` erase mutability (fine for codegen — pointers; loses a possible
  `&mut` `noalias` hint); `Fn`/`Union` are representation-ambiguous (closure `{fn,env}` vs
  fn-ptr; tagged union) — resolved at HIR/MIR lowering. **`Unknown` (lenient checker) must
  be concrete by codegen** — Phase 5 either rejects residual `Unknown` or models more stdlib.

- [x] 2.1 Extract `Ty` into `src/ty.rs` (shared by `typeck`/`borrowck`/`hir`) — moved `Ty`/`IntKind`/`FloatKind` + `impl Ty` helpers + `display_ty`/`int_kind`/`ty_is_copy`, now `pub(crate)`; `typeck` glob-imports them (`use crate::ty::*`), submodules unchanged. Pure refactor, no behavior change.
- [x] 2.2 Name resolution → unique `BindingId` per binding site + a resolution table (use → binding); shadowing resolved here. The resolver now keeps `scopes: Vec<HashMap<String, BindingId>>`, allocates a fresh `BindingId` per `let`/param/pattern-binding/closure-param, and records each local `Ident`/`self` *use* (by `NodeId`) → its binding (globals/builtins resolve by name, no id). Exposed as `checker::resolve(prog) -> Resolutions` + debug command `la3 resolve`. Battery: `tests/resolve.rs` (6 tests, incl. shadowing & inner-scope).
- [x] 2.3 Define `hir.rs` (typed, `BindingId`-based) + `hir::lower(prog, &TypeTable, &Resolutions)` — typed HIR tree (`Ty` embedded in every `HExpr`, taken from `TypeTable::ty_of`), local uses are `Local(BindingId)` / globals `Global(name)` (via `Resolutions::binding_of`). Binding *sites* (no `NodeId`) recover their id with a sequential counter that mirrors name resolution's allocation order, guarded by a `debug_assert` against `Resolutions::name`. A standalone `TyResolver` resolves param/field/return `TypeExpr`s (no `NodeId`) to `Ty`, mirroring `typeck`'s `resolve_in`. **Sugar is lowered 1:1 for now** (`FStr`/`Coalesce`/compound `Assign`/`WhileLet`/`optional`/`Try` retained) — the desugarings are subpart 2.4. Debug command `la3 hir`. Battery: `tests/hir.rs` (9 tests).
- [x] 2.4 Desugarings (in `hir::lower`, so HIR has **no surface sugar**): f-strings → `+`-concat of `Str` + `HExprKind::Format{value,spec}` primitive; `a ?? b`/`a?.x` → `match` on `nil`; `e?` → `match` that unwraps + early-returns (`Result`→`Ok(v)=>v / Err(x)=>return Err(x)`, `Option`/bare → `Some(v)=>v / None=>return nil`, matching the interpreter oracle); compound `x += e` → `x = x + e`; `while let P = e {..}` → `loop { match e { P => .., _ => break } }`; typed `for` kept (step lowered in MIR). Removed the sugar `HExprKind` variants (`FStr`/`Coalesce`/`Try`/`WhileLet`, `optional` flags, compound `op`). Desugar temporaries get **fresh synthetic `BindingId`s** (`Lower::fresh`, based at `Resolutions::binding_count()`) so they never collide with real bindings. **Decisions recorded below.** Battery extended in `tests/hir.rs` (7 new, 16 total).
  > **Decisions (2026-06-04).** (1) **`if let` is not in the grammar** — `parse_if` doesn't accept it (only `parse_while` accepts `while let`), so there is nothing to desugar; left as-is per "deviations from the reference are intentional design". (2) **`?` on Option/None early-returns `nil`, not `None`** — the interpreter (oracle) returns `Value::Nil`, and differential testing binds compiled behavior to the interpreter; the reference's "returns None" is the looser statement. (3) **compound `+=` re-evaluates the place** (`x = x + e`), which double-evaluates a non-trivial place's sub-expressions (e.g. the index in `a[i] += 1`); the interpreter reads the place once. Assignment targets are simple lvalues, and "evaluate the place once" is properly a MIR concern (explicit places/temporaries) — revisit there if a differential test ever surfaces it.
- [x] 2.5 Explicit closures + captures (by-ref vs `move`) in HIR — every `HExprKind::Closure` now carries a `captures: Vec<HCapture>` (binding + `Ty` + `CaptureMode::{Ref,Value}`). Computed over the **lowered** body by `capture_set` (id-based, exact — no name/shadowing reasoning): one walk collects every binding introduced inside the closure (params + nested `let`/pattern/closure-params) and every local *use* with its type; a use whose binding is bound outside is a capture. Mode is uniform per closure — by-ref by default, by-value for `move` (reference Section 6). Captures propagate outward through nested closures; `self` is captured like any other local; nested `fn`/`const` items are separate scopes and don't contribute (documented). Shown in `la3 hir` (`captures(& #n: ty, move #m: ty)`). Battery extended in `tests/hir.rs` (6 new, 22 total).

## Phase 3 — MIR (the layer that makes the rest viable) · STATUS: [~] in progress

A CFG of basic blocks with explicit temporaries and typed locals. **Every hard
transformation happens here**, so Phase 5 (LLVM) stays a thin translation.

> **Re-evaluated 2026-06-04 (user decisions).** Three changes from the original
> numbering — end state unchanged, but the order now follows the real dependency
> graph (lower to MIR, then run MIR passes, as Rust does):
>
> 1. The **HIR→MIR lowering** (listed last originally) is pulled to **3.2** — it is
>    the substrate every other pass needs (no MIR to transform until it exists).
> 2. **Monomorphization moved out to Phase 7** (Generics & interfaces). No milestone
>    example is generic, so it is YAGNI for the first binary; mono is still a MIR
>    pass, just scheduled when generics are actually exercised.
> 3. **`match` gets its own subpart (3.3)**, separate from the core lowering: 2.4
>    desugared `?`/`??`/`?.`/`while let` into `match`, so it is pervasive and worth
>    compiling properly (guards, ranges, `@`, or-patterns, exhaustive default).
>
> **Phase 3 milestone:** `fib`, `fizzbuzz`, `shapes` lower to valid, dumpable MIR
> (`la3 mir`) — mirrors the Phase 5 LLVM milestone. So drop insertion for
> `fizzbuzz`'s strings is in scope; generics are not. (`fib` needs only the core
> lowering 3.2; `fizzbuzz`'s `main` too, but its `classify` and all of `shapes`
> use `match`, so the milestone completes with 3.3.)

- [x] 3.1 Define `mir.rs` (data model only): Rust-MIR-like — `MirProgram`/`MirFn` with **typed locals** (`_0` = return, `_1..` = args, then user/temp, each a `LocalDecl{ty,kind,source,name}`) and a vector of `BasicBlock`s, each a run of `Statement`s + one `Terminator`. `Place` (local + `Projection::{Field,Index,Deref,Downcast}`), `Operand::{Copy,Move,Const}` (the ownership info 3.5 threads), `Rvalue::{Use,Binary,Unary,Cast,Ref,Aggregate}`, `Terminator::{Goto,Return,If,Switch,Call,Unreachable}` (Switch = the match-tree substrate for 3.3). **Explicit drop points** via `Statement::Drop` (inserted by 3.5). Plus a `MirBuilder`, a Rust-MIR-flavoured pretty-printer, and a **`validate`** pass (`MirFn`/`MirProgram`) — an internal ICE-detector that checks the structural invariants (entry block exists, `_0` is the return slot and `_1..` are args, every referenced `Local`/`BlockId` is in range) so a bad lowering/transform is caught at its source; 3.2+ run it on their output. **No HIR→MIR lowering and no `la3 mir` command yet** — those are 3.2; the model is exercised by a hand-built `#[cfg(test)]` battery in `mir.rs` (8 tests). `#![allow(dead_code)]` (like `ast.rs`): the model is wired to producers across 3.2–3.6.
- [x] 3.2 **Lower HIR → MIR CFG** (the substrate) — new `src/mirgen.rs`: `mirgen::lower(&Hir) -> {program, skipped}`. Builds the CFG per function (a `FnLower` threads a `cur` block, a `BindingId→Local` map, and a loop-context stack): straight-line via `lower_operand`/`lower_rvalue`/`lower_place` (literals→`Const`, `Binary`/`Unary`/`Cast`, `Field`/`Index`/`Deref` places, `Assign`, `let`-binding, `Format`→`std::format` call, `Tuple`/`StructLit`→`Aggregate`), and control flow (`if`/`loop`-with-break-value/`while`/`for`-over-range→counter loop/`break`/`continue`/`return`) into terminators. Calls become `Terminator::Call` (module method `io.println`→`io.println`, value method→`Type::method`). **Deferred constructs make a function _bail_ (reported as skipped with a reason), not a broken stub:** `match`→3.3, closures→3.4, heap-collection literals→runtime, async/spawn/try→9/10. Ownership is **not** threaded yet — reads are `Copy` placeholders (3.5 refines to `Move`/`Ref` + inserts drops). Every emitted fn passes `MirFn::validate`. Adds the `la3 mir` command. **`fib` lowers completely** (milestone); `fizzbuzz` `main` lowers, `classify` waits on 3.3. Battery: `tests/mir_lower.rs` (10 tests).
- [ ] 3.3 **Match → decision trees** (its own subpart) — guards, ranges, `@`, or-patterns, exhaustive default; lowers `match` (pervasive after the 2.4 desugarings) into the `Switch`-based CFG. **Targets `shapes`** of the milestone.
- [ ] 3.4 **Closure conversion** — closures → `{fn ptr, captured env}`; `move` vs by-ref capture made explicit (consumes the Phase 2.5 `HCapture` list)
- [ ] 3.5 **Ownership lowering** — consume the Phase 1.6 borrow-check facts: insert deterministic `drop`s at end-of-scope/last-use, lower `&T`/`&mut T` to pointers, thread moves
- [ ] 3.6 **Borrow-check refinement on the CFG** — the precision gaps Phase 1.6.4 can't do soundly on the AST: **NLL** (borrow liveness over the CFG, ends at last use) and **reborrows** (`&*r`). This is why Rust borrow-checks on MIR; the AST pass stays the sound lexical over-approximation until this lands. _Target behavior is already pinned by `#[ignore]`d tests in `tests/ownership.rs` (`nll__`, `reborrow\__`) — they fail today (`cargo test -- --ignored`) and turn green when this lands; delete their `#[ignore]` then.\_

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

Generics are not exercised by any earlier milestone, so this is where they land —
monomorphization included (moved here from MIR 3.2 on 2026-06-04; it is still a
**MIR pass**, just scheduled when there is generic code to specialize).

- [ ] 7.1 **Monomorphization** — collect concrete generic instances reachable from `main`, emit a specialized MIR copy per instantiation (required for static layout). Runs as a MIR→MIR pass before codegen.
- [ ] 7.2 Interfaces: static dispatch via bounds
- [ ] 7.3 Dynamic dispatch via vtables when needed

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

## Phase 12 — À-la-carte dynamic stdlib · STATUS: [ ] ← Pillar 2; no rush, do it _well_

**Spec: [DYNAMIC_STDLIB.md](DYNAMIC_STDLIB.md)** (authoritative). The subparts below
are the implementation checklist; the design, invariants, and open questions live in
that document.

The flagship that lets one stdlib serve both Pico-class bare-metal and full
PC/Web apps. Prereqs: a working backend + runtime (Phases 4–5) and MIR
reachability (3.2). **These subparts are a design skeleton; the open questions
are pinned so the eventual design is chosen deliberately, not by accident.**

- [ ] 12.1 **Author the stdlib as independent modules** — split the runtime/stdlib
      into small modules, each self-contained (works _alone_ with zero deps), each
      with a manifest of exported symbols + the "capabilities" it provides/wants.
- [ ] 12.2 **Reachability-driven DCE** — from `main`, compute the used-symbol
      closure (reuse the MIR 3.2 monomorphization/reachability walk) and emit only
      reached module code. This alone gives "only what you used" in granular mode.
- [ ] 12.3 **Capability / provider model for opportunistic sharing** — shared
      primitives are weak/overridable; a module ships a private fallback (keeps it
      independent), and when a _canonical provider_ of that capability is already in
      the build, fallbacks fold onto it (linker COMDAT/ICF-style, or a pre-codegen
      rewrite we control on MIR). **Open Qs:** deterministic provider precedence;
      where folding happens (our MIR pass vs the linker); how a module _declares_
      "I can provide / I can consume capability X" without creating an alone-dep.
- [ ] 12.4 **Two build modes** — `--stdlib granular` (independent + opportunistic
      sharing + aggressive DCE; embedded/Pico, smallest binaries) vs the **default**
      monolithic mode (whole stdlib freely interdependent; PC/Web). Wire the mode
      through the driver (11.1).
- [ ] 12.5 **Conformance + size** — a granular build and a monolithic build of the
      same program must be **behaviourally identical** (differential vs the
      interpreter oracle); only size differs. Track binary-size benchmarks per mode.

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
- 2026-06-02 — **Phase 1.6.4 done.** Borrow regions in `borrowck`: a second walk tracks `let`-bound borrows (`Borrow { borrower, place, mutable }`) **lexically** (live to the end of the declaring block — sound pre-NLL). A live `&mut x` forbids any other access to `x`; a live `&x` forbids writes to `x` (`check_access` / `place_root`). Lifetimes: `check_escape` rejects returning `&x`/`&mut x`/`&raw x` of a bare local/param (dangling). Within-call borrows still handled by `check_borrow_conflicts`; a temporary `&mut x` argument doesn't lock (verified against `memory.la3`). Zero false positives across examples. Documented gaps: mutation via a method under a shared borrow (needs `&self`/`&mut self` distinction + builtin mutation table), NLL, reborrows, field-granular borrows. 7 new tests (25 in `tests/ownership.rs`). 113 tests pass. Awaiting review before 1.6.5.
- 2026-06-02 — **Closed the 1.6.4 shared-borrow gap.** `SelfKind` now distinguishes `&self` (`Ref`) from `&mut self` (`RefMut`); the parser records it. New `borrowck::method_mutates` treats a method as an exclusive access to its receiver when the user method takes `&mut self`/`mut self`/`self`, or it's a known built-in in-place mutator (`push`/`pop`/`insert`/`remove`/`extend`/`clear`/`append`/`sort`). So `let r = &v; v.push(4)` is now rejected, while `v.len()` / `&self` methods under a shared borrow stay fine. No example regressions. 4 new/updated tests (28 in `tests/ownership.rs`). 116 tests pass.
- 2026-06-02 — **Field-granular borrows + deferred-gap clarity.** Borrows now track a `Place` (root + projection path of `Field`/`Index`) instead of just the root, with `Place::overlaps` (prefix match; distinct fields are disjoint, any-index overlaps any-index). So `&u.name` no longer locks `u.age`, while a whole-value borrow still locks every field; the conflict message names the held place when it differs. (Fixed an infinite-recursion bug in `access_place_or_recurse` for non-rooted places like `foo().bar`, which had stack-overflowed `shapes`/`tls_record`.) **Recorded clearly in the plan** that NLL and reborrows are _not_ soundly fixable on the AST (textual last-use is unsound under loop back-edges) and are deferred to a borrow-check pass over the MIR CFG (new Phase 3.7) — the same reason Rust borrow-checks on MIR; index-granularity and the builtin-mutator list stay intentionally conservative (Rust-faithful). 5 new field/index tests (32 in `tests/ownership.rs`). 120 tests pass.
- 2026-06-02 — **Pinned the deferred gaps with pending tests.** Added three `#[ignore]`d tests in `tests/ownership.rs` (`nll_shared_borrow_dead_before_mutation_is_ok`, `nll_sequential_mut_borrows_are_ok`, `reborrow_releases_the_parent_after_use`) that assert the _correct_ NLL/reborrow behavior. They fail today (`cargo test -- --ignored`, all rejected with aliasing-xor-mutability) and become green once Phase 3.7's MIR borrow-check lands — at which point the `#[ignore]` comes off. Default suite stays green (3 ignored). 120 tests pass + 3 ignored.
- 2026-06-02 — **Phase 1.6.5 done → Phase 1.6 complete.** Added the drop classification `TypeChecker::ty_needs_drop` (heap-owning built-ins + any aggregate that transitively owns one; scalars/refs/pointers/slices/`fn` don't), surfaced as `drop=yes/no` per struct/enum in `la3 layout` and pinned by `tests/drops.rs` (8 tests). Wrote the **drop contract** MIR 3.5 must honour into the plan (what/when/skip-moved/partial-moves/what-MIR-carries). 128 tests pass, 0 warnings. **Phase 1.6 (ownership & borrow checker) is now complete** — move semantics + use-after-move, argument/receiver/move-closure moves, lexical borrow exclusivity (field-granular, method-mutation aware), dangling-return lifetimes, and the drop contract; NLL + reborrows are explicitly deferred to Phase 3.7 (MIR), pinned by `#[ignore]`d tests. Awaiting review before Phase 2 (HIR).
- 2026-06-02 — **Phase 2.1 done.** Extracted the semantic type into `src/ty.rs` (`Ty`/`IntKind`/`FloatKind` + `impl Ty` helpers + `display_ty`/`int_kind`/`ty_is_copy`), now `pub(crate)` and shared. `typeck` glob-imports it (`use crate::ty::*`); its submodules keep `use super::*` (the glob re-export chains through). Pure mechanical refactor — `cargo build` clean (0 warnings), 128 tests pass + 3 ignored, `la3 types`/`layout` unchanged. Sets up HIR (2.3) to embed `Ty` directly. Awaiting review before 2.2.
- 2026-06-02 — **`Ty` review (post-2.1).** Confirmed/recorded: `str` is **owned** (String-like, dropped), `&[u8]` is the borrowed form; derived **`Eq + Hash`** on `Ty`/`IntKind`/`FloatKind` for MIR 3.2 monomorphization keying; documented the `IntLit`/`FloatLit`-never-in-HIR invariant and the deferred items (`Ref`/`Ptr` mut-erasure, `Fn`/`Union` representation, `Unknown` must be concrete by codegen). Doc comments added to `src/ty.rs`. 128 tests pass + 3 ignored, 0 warnings.
- 2026-06-04 — **Phase 3.1 done.** New `src/mir.rs`: the MIR data model (Rust-MIR-like) — `MirProgram`/`MirFn` with typed locals (`_0` return, `_1..` args, then user/temp via `LocalDecl`+`LocalKind`), `BasicBlock` = `Statement`s + one `Terminator`; `Place`/`Projection`, `Operand::{Copy,Move,Const}`, `Rvalue::{Use,Binary,Unary,Cast,Ref,Aggregate}`, `Terminator::{Goto,Return,If,Switch,Call,Unreachable}`, explicit `Statement::Drop` points. Added a `MirBuilder` + a Rust-MIR-flavoured printer. **Scope decision:** 3.1 is the *definition* only (the plan splits the lowering across 3.2–3.6); no HIR→MIR lowering and no `la3 mir` command yet — both arrive at 3.6 with the CFG machinery. Exercised by a hand-built `#[cfg(test)]` battery (5 tests). `#![allow(dead_code)]` per the `ast.rs` precedent (model wired up over the phase). 162 tests pass (+5) + 3 ignored, 0 warnings; interpreter oracle unchanged. Awaiting review before 3.2.
- 2026-06-04 — **Phase 3.2 done.** New `src/mirgen.rs` lowers HIR → MIR CFG (the substrate). A per-function `FnLower` threads a current block + a `BindingId→Local` map + a loop-context stack, lowering straight-line code (`lower_operand`/`lower_rvalue`/`lower_place`) and control flow (`if`/`loop`-with-break-value/`while`/`for`-over-range as a counter loop/`break`/`continue`/`return`) into MIR terminators; calls become `Terminator::Call`. Deferred constructs (`match`→3.3, closures→3.4, heap literals→runtime, async→9/10) make a function **bail** and be reported as skipped — no broken stubs in the MIR. Ownership isn't threaded yet (reads are `Copy` placeholders; 3.5 refines). Every emitted fn passes `MirFn::validate`. New `la3 mir` command. **`fib` lowers completely** (the 3.2 half of the milestone); across all 13 examples `la3 mir` produces zero invalid MIR. Battery `tests/mir_lower.rs` (10 tests). 175 tests pass (+10) + 3 ignored, 0 warnings; interpreter oracle unchanged. Awaiting review before 3.3.
- 2026-06-04 — **Phase 3 re-evaluated + MIR validator added (user-directed).** Re-ordered Phase 3 to follow the real dependency graph: HIR→MIR lowering pulled to **3.2** (the substrate), **`match` given its own subpart (3.3)** (pervasive after the 2.4 desugarings), and **monomorphization moved out to Phase 7.1** (no milestone example is generic). Pinned a Phase-3 milestone (`fib`/`fizzbuzz`/`shapes` → dumpable MIR). Fixed the stale Pillar 2 hook (DCE leans on the **MIR reachability walk**, available at 3.2; mono is 7.1). Added a **`validate`** pass to `mir.rs` (`MirFn`/`MirProgram`) — an internal invariant checker (entry exists, `_0`=return/`_1..`=args, all `Local`/`BlockId` refs in range) that 3.2+ run on their output; 3 new tests (8 in `mir.rs`). 165 tests pass + 3 ignored, 0 warnings.
- 2026-06-04 — **Phase 2 review (user-requested).** Critical pass over the HIR/desugar/capture code, focused on bugs the structural tests can't catch (no differential yet). Findings: (a) the type-directed `e?` is robust on the real corpus — every `?` operand types as `Enum("Result", …)` even when the payload is `Unknown` (`Result<_>`), so the Result-form is chosen correctly; (b) the **Option** form of `?` (`Some(v)=>v / None=>return nil`) was unexercised by examples/tests — verified it lowers and the interpreter oracle runs it (`pick(Some(5))→6`), and **added a regression test** (`try_on_option_…`, hir suite 23, total 157). Noted (not fixed — small refinements for MIR): the `?.` synthetic temp is typed `T | nil` rather than the stripped `T` (value is correct, only the temp's declared type is loose); closure captures collapse `&`/`&mut` to one `Ref` mode (mut-ness is re-derived on the MIR, consistent with the `Ty` mut-erasure decision). No correctness defect found. 157 pass, 0 warnings.
- 2026-06-04 — **Phase 2.5 done → Phase 2 complete.** Closure captures are now explicit in HIR: `HExprKind::Closure` carries `captures: Vec<HCapture>` (binding + `Ty` + `CaptureMode::Ref|Value`), computed by `capture_set` over the **lowered** body using `BindingId`s (exact, no shadowing reasoning) — free = local uses whose binding is bound outside the closure; mode is by-ref by default, by-value for `move` (reference §6). Verified by-ref/`move`/no-capture/param-shadowing/nested-propagation/`self`-capture via `la3 hir`. 156 tests pass (+6 in `tests/hir.rs`, 22 total) + 3 ignored, 0 warnings; interpreter oracle unchanged. **Phase 2 (HIR + desugaring) is complete** — typed `BindingId`-based HIR, all surface sugar desugared, explicit captures. Next is Phase 3 (MIR). Awaiting review before Phase 3.
- 2026-06-04 — **North Star recorded (no code).** Captured two durable pillars from the user: (1) one language for low-level (Pico bootloader/kernel) **and** high-level (web/PC) — the front-end already fits both; the split is runtime/target. (2) The **à-la-carte dynamic stdlib** — explicitly **not** `no_std`: many small fully-independent modules, only the used ones linked, with **opportunistic sharing** when two co-present modules can dedupe (never creating a dependency in the alone case), plus a granular vs monolithic build mode. Added a "North Star" section + a dedicated **Phase 12** skeleton with the open design questions pinned. No rush; to be done well.
- 2026-06-04 — **Phase 2.4 done.** Desugarings now run inside `hir::lower`, so HIR carries **no surface sugar**: f-strings → `+`-concat of `Str` literals and a new `HExprKind::Format{value,spec}` primitive (runtime honours the spec in 4.3); `??`/`?.` → `match` on `nil`; `e?` → type-directed `match`+early-return (`Result` reconstructs `Err`, `Option`/bare returns `nil` — matching the interpreter oracle, not the reference's "None"); `+=` → `x = x + e`; `while let` → `loop { match … _ => break }`. Removed the now-dead sugar variants (`FStr`/`Coalesce`/`Try`/`WhileLet`, the `optional` flags, the compound `op`). Desugar temporaries get fresh **synthetic** `BindingId`s (`Lower::fresh`, based at the new `Resolutions::binding_count()`), so the real-id alignment assertion still holds across all 13 examples. Recorded three decisions in 2.4 (no `if let` in the grammar; `?`-on-None → `nil`; `+=` re-evaluates the place — a MIR concern). 150 tests pass (+7 in `tests/hir.rs`, 16 total) + 3 ignored, 0 warnings. Interpreter oracle unchanged. Awaiting review before 2.5.
- 2026-06-04 — **Phase 2.3 done.** New `src/hir.rs`: a typed, `BindingId`-based HIR + `hir::lower(prog, &TypeTable, &Resolutions)`. Every `HExpr` embeds its `Ty` (from a new `TypeTable::ty_of`), local uses are `Local(BindingId)` / globals `Global(name)` (from `Resolutions::binding_of`), and binding *sites* (which carry no `NodeId`) recover their id with a sequential counter that mirrors name resolution's pre-order allocation walk — a `debug_assert_eq!` against `Resolutions::name` makes any drift loud (held across all 13 examples). A standalone `TyResolver` lowers param/field/return `TypeExpr`s to `Ty` (mirrors `typeck::resolve_in` over the program's nominal decls). Lowering is faithful 1:1; the listed **desugarings stay for 2.4**, so sugar variants (`FStr`/`Coalesce`/compound `Assign`/`WhileLet`/`?.`-`optional`/`Try`) are retained. New `la3 hir` debug command + battery `tests/hir.rs` (9 tests). 143 tests pass + 3 ignored, 0 warnings. Awaiting review before 2.4.
- 2026-06-02 — **Phase 2.2 done.** Name resolution now assigns a unique `BindingId` (new in `ast.rs`) to every value binding site and maps each local `Ident`/`self` use → its binding, with scopes as `Vec<HashMap<String, BindingId>>`. Shadowing is resolved here, once (proven by `la3 resolve` on `let x; let y=x; let x`: the two uses of `x` target `#0` vs `#2`). Globals/builtins still resolve by name (no id). New `checker::resolve(prog) -> Resolutions` (used by `check`), debug command `la3 resolve`, and battery `tests/resolve.rs` (6 tests). 134 tests pass + 3 ignored, 0 warnings. Sets up HIR (2.3) to be `BindingId`-based. Awaiting review before 2.3.

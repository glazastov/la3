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
| Memory         | **ARC** (automatic reference counting), no full borrow checker      | Doable in weeks; sound enough                |
| Generics       | Monomorphization                                                    | Required for static layout                   |
| IR             | AST → HIR (typed, desugared) → MIR (mono, layout, RC, match) → LLVM | The layer that makes the rest viable         |
| Test oracle    | The interpreter stays alive for _differential testing_              | Cheap confidence per phase                   |
| Initial target | `x86_64-unknown-linux-gnu` native                                   | Reduces scope                                |
| Toolchain      | rustup `stable` (≥1.94), edition 2024                               | pinned by `rust-toolchain.toml`              |

### v1 subset (explicit cuts for the first binary)

Deferred to late/future phases: `async`/`await`/`all`/`race`, `try`/`catch` (unwinding),
full borrow checker, real `net`/`crypto`. `Result`/`Option`/`?` land early (just enums).

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

## Phase 1 — Sound type checker · STATUS: [~] in progress

Prerequisite for everything. The light `typeck` becomes the source of truth.

- [x] 1.1 Annotate **every AST node** with a concrete `Ty` — `NodeId` on `Expr` ([ast.rs](src/ast.rs)), `Program::assign_ids`, `typeck::check_types` → `TypeTable`, debug `la3 types`
- [ ] 1.2 Full field & method resolution (struct/enum/builtin) → error if unresolved
- [ ] 1.3 _Layout_ computation: structs, **tagged unions** for enums (incl. `Option`/`Result`), tuples, `[T;N]`
- [ ] 1.4 Exact `as` semantics: truncation, sign, `**`→`f64`, `//` floor, `/` trunc-toward-zero
- [ ] 1.5 Sound inference (`i32`/`f64` defaults, no implicit widening) + real type errors

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

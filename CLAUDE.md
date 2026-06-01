# CLAUDE.md — Working rules for the La3 codebase

La3 (Laila Lang) is a small language. The repo currently ships a lexer, parser,
checker, light type checker, and a tree-walking interpreter. We are building a
real **LLVM compiler** on top of it. The roadmap and live progress live in
[COMPILER_PLAN.md](COMPILER_PLAN.md) — read it first, every session.

## Golden workflow (non-negotiable)

1. **Read [COMPILER_PLAN.md](COMPILER_PLAN.md)** — find the current phase and the next unchecked subpart.
2. Work **one subpart at a time**.
3. After each subpart: **build → test → verify**, then tick its checkbox in the plan. **Every phase must ship at least one dedicated battery of tests** for what it added (in `tests/` or `#[cfg(test)]`).
4. When a whole phase is done: update its `STATUS`, add a line to the Progress log, and **stop for the user to review**. Do not start the next phase unprompted.
5. **Always consult the language reference** ([laila-lang-reference.md](laila-lang-reference.md)) for the authoritative semantics before implementing a feature.
6. **Always keep [README.md](README.md) up to date** as user-facing behavior, commands, or implemented features change.

## Toolchain (important)

- The project uses rustup `stable` (1.96+, edition 2024), pinned via `rust-toolchain.toml`.
- The old Ubuntu system Rust (1.75, `apt` packages `rustc`/`cargo`) has been **removed**, so
  plain `cargo`/`rustc` now resolve to the rustup toolchain. Just use `cargo` directly.
- If you ever see `feature edition2024 is required ... Cargo 1.75.0`, the old system Rust came
  back (e.g. `apt install rustc`); remove it again with
  `sudo apt-get remove -y rustc cargo libstd-rust-dev && sudo apt-get autoremove -y`.

## Build / test / verify commands

```sh
cargo build --workspace             # build crate + runtime
cargo test --workspace              # run all tests (tests/*.rs + runtime)
cargo run -- run examples/fib.la3      # run a program in the interpreter
cargo run -- check examples/fib.la3    # type/name check only
cargo run -- build examples/fib.la3    # (WIP) compile to a native binary
```

"Verify" means: run the relevant example(s) and confirm behavior — and once the
compiler emits binaries, **differentially test** the compiled output against the
interpreter (same stdout/exit code).

## LLVM (from Phase 4 on)

- LLVM 18 is installed at `/usr/lib/llvm-18` (not on `PATH`).
- `inkwell` needs: feature `llvm18-0` and env `LLVM_SYS_181_PREFIX=/usr/lib/llvm-18`.
- `inkwell` is intentionally **not** a dependency yet (keeps the build green); it
  is added in Phase 4.1.

## Architecture (target)

```
AST → [sound type check] → HIR (typed, desugared)
    → [monomorphization] → MIR (layout, RC, match lowering) → LLVM IR → object → link runtime
```

- Keep the **interpreter** working — it is our correctness oracle.
- Heap memory uses **ARC** in v1 (no full borrow checker yet).
- The **`runtime/`** crate is the native runtime the compiled code links against.

## Don't guess — read the docs

When a detail is uncertain (a Rust edition-2024 change, an `inkwell`/LLVM API, an
LLVM IR semantic, a `llvm-sys` env var), **look it up in the official docs**
instead of assuming. Prefer primary sources: the Rust reference/edition guide,
the LLVM Language Reference, and the `inkwell`/`llvm-sys` docs. Record any
non-obvious decision in `COMPILER_PLAN.md`.

## Code conventions

- Match the surrounding style: same naming, comment density, and idioms as the existing `src/*.rs`.
- Keep diagnostics going through [src/diag.rs](src/diag.rs) so errors point at source spans.
- The language spec is authoritative: [laila-lang-reference.md](laila-lang-reference.md). When semantics
  are ambiguous there, record the chosen behavior in `COMPILER_PLAN.md`.

## Source map

| File                             | Role                                               |
| -------------------------------- | -------------------------------------------------- |
| [src/lexer.rs](src/lexer.rs)     | Tokenizer                                          |
| [src/parser.rs](src/parser.rs)   | Recursive-descent parser → AST                     |
| [src/ast.rs](src/ast.rs)         | AST node definitions                               |
| [src/checker.rs](src/checker.rs) | Name-resolution pass                               |
| [src/typeck.rs](src/typeck.rs)   | Type checker; semantic `Ty` (submodules in `typeck/`) |
| [src/borrowck.rs](src/borrowck.rs) | Ownership / borrow checker (Phase 1.6)           |
| [src/interp.rs](src/interp.rs)   | Tree-walking interpreter (submodules in `interp/`) |
| [src/diag.rs](src/diag.rs)       | Diagnostics with source spans                      |
| [src/main.rs](src/main.rs)       | CLI: `run`/`check`/`ast`/`tokens` (+ `build`, WIP) |

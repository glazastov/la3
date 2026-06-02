# La3

A lexer, parser, checker, tree-walking interpreter, and **(in progress) LLVM compiler** for **Laila Lang (La3)**, the reading pseudo-language documented at [glazastov.com](https://glazastov.com). La3 borrows the clearest parts of Rust, C, TypeScript, and Lua to make code examples readable regardless of which real language you know best.

The interpreter is written in Rust with no external dependencies. The native compiler is being built on top of it — the interpreter stays as the correctness oracle. The roadmap and live progress are in [COMPILER_PLAN.md](COMPILER_PLAN.md).

## Build

```bash
cargo build --release
```

## Usage

```bash
la3 run    <file.la3>    # parse, check, and execute (calls `main`)
la3 check  <file.la3>    # parse and report undefined-name and type errors
la3 ast    <file.la3>    # parse and print the AST
la3 tokens <file.la3>    # print the token stream
la3 types  <file.la3>    # print the inferred type of every expression
la3 layout <file.la3>    # print the by-value byte layout of structs and enums
la3 build  <file.la3>    # compile to a native binary (WIP — see COMPILER_PLAN.md)
```

Arguments after the file are passed to the program and read with `os.args()`:

```bash
cargo run -- run examples/cli_args.la3 -- alice bob carol
```

## Examples

The [`examples/`](examples/) directory has runnable programs:

| File              | Shows                                                |
| ----------------- | ---------------------------------------------------- |
| `fib.la3`         | recursion, `if` as an expression, ranges             |
| `shapes.la3`      | structs, `impl`, enums with data, exhaustive `match` |
| `options.la3`     | `Option`, `Result`, the `?` operator, `??`           |
| `collections.la3` | lists, closures, `map`/`filter`/`reduce`, maps, sets |
| `fizzbuzz.la3`    | tuple `match`, `loop { break value }`                |
| `tls_record.la3`  | the reference manual's TLS encode/decode example     |
| `json.la3`        | `json.encode` / `json.decode` / `json.pretty`        |
| `file_read.la3`   | `fs.read` returning `Result<str>`                    |
| `cli_args.la3`    | `os.args()`, `os.env()`                              |
| `word_count.la3`  | file read, tokenize, count, `sort_by`                |
| `http_server.la3` | an in-process router over modeled requests           |
| `channels.la3`    | channels, `spawn`/`join`, `await all`/`race`         |

```bash
cargo run -- run examples/shapes.la3
```

## What is implemented

- **Lexer**: comments, numeric bases and suffixes, char/string/f-string literals.
- **Parser**: items (`fn`, `struct`, `enum`, `impl`, `interface`, `const`, `type`, `use`, `mod`), a Pratt expression parser, patterns, and optional semicolons via significant-newline filtering.
- **Checker**: name resolution (reports undefined names) followed by a static **type checker** covering reference Sections 2, 4, 7, and 9 — type inference for `let`/`const`, the `i32`/`f64` literal defaults, no implicit numeric conversion (an `as` cast is required), `as` cast legality (only numeric↔numeric and integer↔`char`, so `str as i32`/`bool as f64` are rejected), the `nil` / `Option<T>` identity, operator typing (`**` yields `f64`, comparison/logical yield `bool`, bitwise needs integers, `??`/`?.` on `T | nil`), `if`/`match` arm agreement, `match` exhaustiveness, the `?`-operator context rule, struct-literal field checking, field and method resolution (an unknown field on a known struct/tuple, or an unknown method on a fully-modeled receiver, is reported with a span), and nominal interface conformance for generic bounds (`T: Iface` needs an explicit `impl`). A third pass, the **borrow checker** (reference Section 11), then enforces ownership: moving a non-`Copy` value out of a binding — by `let y = x`, by passing it to a by-value parameter, or by calling a `self`/`mut self` method — and using it afterward is a **use-after-move** error (flow-sensitive across `if`/`match`/loops). `&T`/`&mut T` and the built-in stdlib borrow rather than move, and a `move` closure takes ownership of the non-`Copy` values it captures. It also enforces **borrow exclusivity** — a `let`-bound `&mut x` forbids any other access to `x` while live, and a `&x` forbids writes (aliasing xor mutability) — and rejects **dangling references** (returning `&x` of a local). It is being built up over Phase 1.6; deterministic drop / the MIR ownership-lowering contract lands next.
- **Interpreter**: immutable-by-default bindings, closures, recursion, generics (erased), `match` with guards/ranges/bindings, `if`/`while`/`while let`/`for`/`loop`, structs and methods, enums with data, `Option`/`Result` with `?`, tuples, lists, maps, sets, ranges, f-strings with format specs, **concurrency** (channels with `send`/`recv`/`close`/iteration, `spawn`/`join`, and `await all`/`race` over a cooperative scheduler), and a standard-library subset (`io`, `fs`, `os`, `json`, `math`, `bytes`, free `str`/`len`/`min`/`max`/`to_hex`/`from_hex`).

## Native compiler (in progress)

A real LLVM back-end is being built on top of the front-end; see [COMPILER_PLAN.md](COMPILER_PLAN.md) for the phased roadmap and status. The pipeline target is `AST → sound type + borrow check → HIR → MIR → LLVM IR → object → link runtime`, with memory managed by Rust-style **ownership and a borrow checker** (move semantics + deterministic drop; this supersedes the earlier ARC plan). **MIR** is an explicit phase that owns the hard lowerings (monomorphization, match decision trees, closure conversion, drop insertion), keeping the LLVM back-end a thin translation. The workspace now includes a [`runtime/`](runtime/) crate (the native runtime compiled programs will link against). As of Phase 1, the type checker annotates every expression node with a concrete type (`la3 types`) and computes the by-value memory layout of structs and enums (`la3 layout`) — both consumed by the back-end.

## Deliberate deviations from the spec

These are points where the written language has an ambiguity or a feature this interpreter resolves pragmatically. They are called out so behavior is never surprising.

- **`//` is always a line comment.** The reference overloads `//` for both line comments (from C/Rust) and floor division (from Lua); a lexer cannot tell `7 // 2` from `x // note` apart. Floor division is the `idiv(a, b)` builtin instead.
- **`nil` and `None` are the same runtime value**, exactly as the spec states; `Some`/`Ok`/`Err` are tagged values.
- **Concurrency is cooperative, not parallel.** The interpreter is single-threaded (its value model is `Rc`-based and the project takes no dependencies, so OS-thread parallelism and preemptive coroutines are out of scope). Instead, `spawn` defers a task that runs to completion the first time its result is needed: a `join`, an `await`, a `recv` that finds the channel empty, or program shutdown (so fire-and-forget tasks still run). A blocked `recv` drives the scheduler, so a producer task runs before the consumer retries; this gives correct producer/consumer ordering and real interleaving at the task level, though not mid-task preemption. `await all(...)` resolves every task in order and `await race(...)` takes the first. Channel `capacity` is therefore advisory (the buffer is never bounded in a way that could deadlock a single-threaded run), and a `recv` from an empty channel that no runnable task can fill or close is reported as a deadlock.
- **`move` closures behave like ordinary closures.** Captures are by reference; ownership transfer is not modeled.
- **Types are checked lightly.** Annotations are parsed and mostly informational; there is no full static type system in v0.1.

## Tests

```bash
cargo test
```

## License

MIT.

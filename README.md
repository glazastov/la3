# La3

A lexer, parser, checker, and tree-walking interpreter for **Laila Lang (La3)**, the reading pseudo-language documented at [glazastov.com](https://glazastov.com). La3 borrows the clearest parts of Rust, C, TypeScript, and Lua to make code examples readable regardless of which real language you know best.

This implementation is written in Rust with **no external dependencies** (only the standard library).

## Build

```bash
cargo build --release
```

## Usage

```bash
la3 run    <file.la3>    # parse, check, and execute (calls `main`)
la3 check  <file.la3>    # parse and report undefined-name errors
la3 ast    <file.la3>    # parse and print the AST
la3 tokens <file.la3>    # print the token stream
```

Arguments after the file are passed to the program and read with `os.args()`:

```bash
cargo run -- run examples/cli_args.la3 -- alice bob carol
```

## Examples

The [`examples/`](examples/) directory has runnable programs:

| File | Shows |
| ---- | ----- |
| `fib.la3` | recursion, `if` as an expression, ranges |
| `shapes.la3` | structs, `impl`, enums with data, exhaustive `match` |
| `options.la3` | `Option`, `Result`, the `?` operator, `??` |
| `collections.la3` | lists, closures, `map`/`filter`/`reduce`, maps, sets |
| `fizzbuzz.la3` | tuple `match`, `loop { break value }` |
| `tls_record.la3` | the reference manual's TLS encode/decode example |
| `json.la3` | `json.encode` / `json.decode` / `json.pretty` |
| `file_read.la3` | `fs.read` returning `Result<str>` |
| `cli_args.la3` | `os.args()`, `os.env()` |
| `word_count.la3` | file read, tokenize, count, `sort_by` |
| `http_server.la3` | an in-process router over modeled requests |

```bash
cargo run -- run examples/shapes.la3
```

## What is implemented

- **Lexer**: comments, numeric bases and suffixes, char/string/f-string literals.
- **Parser**: items (`fn`, `struct`, `enum`, `impl`, `interface`, `const`, `type`, `use`, `mod`), a Pratt expression parser, patterns, and optional semicolons via significant-newline filtering.
- **Checker**: conservative name resolution (reports undefined names).
- **Interpreter**: immutable-by-default bindings, closures, recursion, generics (erased), `match` with guards/ranges/bindings, `if`/`while`/`while let`/`for`/`loop`, structs and methods, enums with data, `Option`/`Result` with `?`, tuples, lists, maps, sets, ranges, f-strings with format specs, and a standard-library subset (`io`, `fs`, `os`, `json`, `math`, `bytes`, free `str`/`len`/`min`/`max`/`to_hex`/`from_hex`).

## Deliberate deviations from the spec

These are points where the written language has an ambiguity or a feature this interpreter resolves pragmatically. They are called out so behavior is never surprising.

- **`//` is always a line comment.** The reference overloads `//` for both line comments (from C/Rust) and floor division (from Lua); a lexer cannot tell `7 // 2` from `x // note` apart. Floor division is the `idiv(a, b)` builtin instead.
- **`nil` and `None` are the same runtime value**, exactly as the spec states; `Some`/`Ok`/`Err` are tagged values.
- **`spawn`, `await`, `all`, `race` run synchronously.** The interpreter is single-threaded, so concurrency primitives execute eagerly and in order, which preserves the observable result of example programs.
- **`move` closures behave like ordinary closures.** Captures are by reference; ownership transfer is not modeled.
- **Types are checked lightly.** Annotations are parsed and mostly informational; there is no full static type system in v0.1.

## Tests

```bash
cargo test
```

## License

MIT.

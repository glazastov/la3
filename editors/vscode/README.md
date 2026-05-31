# La3 for VS Code

Editor support for **Laila Lang (La3)** `.la3` files:

- **Syntax highlighting** from a TextMate grammar derived directly from the
  interpreter's lexer ([`src/lexer.rs`](../../src/lexer.rs)), so keyword,
  operator, literal, and comment rules match what La3 actually parses (line `//`
  and nested `/* */` comments, `f"..."` interpolation with format specs,
  `0x`/`0o`/`0b` literals with numeric suffixes, `**`, `..=`, `?.`, `??`, `->`,
  `=>`, and friends).
- **Live diagnostics** by running the real `la3 check` and underlining its
  lex/parse/type errors as you type. Using the genuine checker means the editor
  and the command line never disagree.
- **Editing support**: comment toggling, bracket matching, auto-closing,
  indentation.
- **Commands**: `La3: Check current file`, `La3: Run current file`,
  `La3: Build interpreter`.

## How diagnostics work

The extension shells out to the `la3` binary (`la3 check <file>`) and parses its
output into editor squiggles. There is no separate language server process and
no runtime dependencies; the extension is TypeScript compiled to a single
`out/extension.js` against the `vscode` API.

It finds the binary in this order:

1. the `la3.path` setting, if set;
2. `target/release/la3`, then `target/debug/la3` under the workspace;
3. `la3` on your `PATH`.

If none is found it offers to build the interpreter for you.

## Settings

| Setting | Default | Meaning |
| ------- | ------- | ------- |
| `la3.path` | `""` | Explicit path to the `la3` binary (overrides auto-detection). |
| `la3.diagnostics.run` | `"onType"` | `onType`, `onSave`, or `off`. |
| `la3.diagnostics.debounce` | `350` | Milliseconds after the last keystroke before re-checking (onType). |

## Try it without packaging

1. Build the interpreter once: `cargo build --release` (from the repo root).
2. Install the extension's build deps: `cd editors/vscode && bun install`.
3. Open this folder in VS Code and press `F5`. The bundled launch config compiles
   the TypeScript first, then opens an Extension Development Host.
4. Open any file under [`examples/`](../../examples). Highlighting and
   diagnostics apply immediately to `.la3` files. Introduce a typo to see a
   squiggle, then save and watch it clear.

## Develop

The extension is written in TypeScript. Install the build-time dependencies and
compile:

```bash
cd editors/vscode
bun install
bun run compile      # or: bun run watch
```

Then press `F5` (see above) to launch it.

## Install from source

Packaging runs the compile step automatically (`vscode:prepublish`):

```bash
cd editors/vscode
bun install
bunx @vscode/vsce package
code --install-extension la3-language-0.2.0.vsix --force
```

## Files

| File                           | Purpose                                               |
| ------------------------------ | ----------------------------------------------------- |
| `package.json`                 | Manifest: language, grammar, commands, settings       |
| `src/extension.ts`             | Diagnostics integration and commands (TypeScript)     |
| `tsconfig.json`                | TypeScript compiler options                           |
| `out/extension.js`             | Compiled output (generated; what VS Code loads)       |
| `language-configuration.json`  | Comments, brackets, auto-closing, indentation         |
| `syntaxes/la3.tmLanguage.json` | The TextMate grammar                                  |

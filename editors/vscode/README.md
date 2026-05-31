# La3 for VS Code

Syntax highlighting and basic editing support (comment toggling, bracket
matching, auto-closing) for **Laila Lang (La3)** `.la3` files.

The grammar is derived directly from the interpreter's lexer
([`src/lexer.rs`](../../src/lexer.rs)), so the keyword, operator, literal, and
comment rules match what La3 actually parses (line `//` and nested `/* */`
comments, `f"..."` interpolation with format specs, `0x`/`0o`/`0b` literals with
numeric suffixes, `**`, `..=`, `?.`, `??`, `->`, `=>`, and friends).

## Install from source

No `npm install` is needed; this is a declarative grammar with no build step.
Package it into a `.vsix` and install:

```bash
cd editors/vscode
bunx @vscode/vsce package
code --install-extension la3-language-0.1.0.vsix
```

Re-running after edits:

```bash
bunx @vscode/vsce package
code --install-extension la3-language-0.1.0.vsix --force
```

## Try it without packaging

Open this folder in VS Code and press `F5` to launch an Extension Development
Host, then open any file under [`examples/`](../../examples). Highlighting
applies immediately to `.la3` files.

## Files

| File | Purpose |
| ---- | ------- |
| `package.json` | Registers the `la3` language and the `.la3` extension |
| `language-configuration.json` | Comments, brackets, auto-closing, indentation |
| `syntaxes/la3.tmLanguage.json` | The TextMate grammar |

# Changelog

All notable changes to the La3 VS Code extension are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/), and the project
uses [Semantic Versioning](https://semver.org/).

## 0.2.2

### Added

- Bundled interpreter. The extension now ships a platform-specific `la3` binary
  under `bin/<platform>-<arch>/`, so a plain install works without building the
  interpreter yourself. The executable bit is restored on first use if the
  `.vsix` extraction dropped it. Regenerate the bundle with `bun run bundle:bin`.

### Changed

- Binary resolution order is now: `la3.path` setting, then a workspace
  `target/release`/`target/debug` build, then the bundled binary, then `PATH`.

## 0.2.1

### Added

- File and extension icons. `.la3` files now show the La3 brand mark in the
  Explorer and tabs (via the per-language `icon` contribution), and the same
  mark is used as the extension's marketplace icon. The source logo's
  transparent padding is trimmed so the icon stays legible at small sizes.

## 0.2.0

### Added

- Live diagnostics. The extension runs the real `la3 check` and underlines its
  lex/parse/type errors, on type (debounced) or on save. Unsaved buffers are
  checked through a temp file so squiggles stay current before saving.
- Commands: `La3: Check current file`, `La3: Run current file`, and
  `La3: Build interpreter`.
- Settings: `la3.path`, `la3.diagnostics.run`, and `la3.diagnostics.debounce`.
- Automatic discovery of the `la3` binary (`la3.path`, then
  `target/release/la3`, then `target/debug/la3`, then `PATH`).

### Changed

- The extension is now authored in TypeScript (`src/extension.ts`) and compiled
  to `out/extension.js`; runtime still has no dependencies beyond the `vscode`
  API and Node's standard library.

## 0.1.0

### Added

- Initial release: TextMate syntax highlighting derived from the interpreter's
  lexer, plus language configuration (comments, brackets, auto-closing,
  indentation) for `.la3` files.

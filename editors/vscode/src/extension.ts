// La3 editor integration.
//
// La3 has no language server of its own; instead this extension shells out to
// the real interpreter (`la3 check <file>`) and turns its diagnostics into
// editor squiggles. Running the genuine checker means the editor and the
// command line never disagree about what is an error.
//
// The extension has no runtime dependencies beyond the `vscode` API and Node's
// standard library, matching the La3 project's no-dependency ethos. TypeScript
// is a build-time-only tool here.

import * as vscode from "vscode";
import * as cp from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

let diagnostics: vscode.DiagnosticCollection;
let output: vscode.OutputChannel;

/** Debounce timers keyed by document URI. */
const timers = new Map<string, ReturnType<typeof setTimeout>>();

/**
 * One rendered diagnostic from `la3` looks like:
 *
 *   type error: cannot find name `foo` (examples/x.la3:12:7)
 *     let y = foo + 1
 *               ^
 *
 * The first line always ends with `(<path>:<line>:<col>)`. We anchor on that
 * trailing position so a message containing parentheses still parses.
 */
const DIAG_RE =
  /^(lex error|parse error|type error|runtime error): (.+) \([^()]*:(\d+):(\d+)\)\s*$/;

export function activate(context: vscode.ExtensionContext): void {
  diagnostics = vscode.languages.createDiagnosticCollection("la3");
  output = vscode.window.createOutputChannel("La3");
  context.subscriptions.push(diagnostics, output);

  // Re-check on the configured trigger.
  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => maybeCheck(doc, false)),
    vscode.workspace.onDidChangeTextDocument((e) => maybeCheck(e.document, true)),
    vscode.workspace.onDidSaveTextDocument((doc) => maybeCheck(doc, false)),
    vscode.workspace.onDidCloseTextDocument((doc) => {
      diagnostics.delete(doc.uri);
      clearTimer(doc.uri.toString());
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("la3.check", () => {
      const doc = vscode.window.activeTextEditor?.document;
      if (doc && doc.languageId === "la3") check(doc);
    }),
    vscode.commands.registerCommand("la3.run", () => runActiveFile()),
    vscode.commands.registerCommand("la3.build", () => buildInterpreter())
  );

  // Check anything already open at startup.
  for (const doc of vscode.workspace.textDocuments) maybeCheck(doc, false);
}

export function deactivate(): void {
  for (const t of timers.values()) clearTimeout(t);
  timers.clear();
}

/**
 * Decide whether this edit/open should trigger a check, honouring the
 * `la3.diagnostics.run` mode. `fromType` is true for live keystroke changes.
 */
function maybeCheck(doc: vscode.TextDocument, fromType: boolean): void {
  if (doc.languageId !== "la3") return;
  const mode = config().get<string>("diagnostics.run", "onType");
  if (mode === "off") return;
  if (fromType && mode !== "onType") return;
  if (!fromType && mode === "onType") {
    // open/save in onType mode: check immediately, no debounce.
    check(doc);
    return;
  }

  const key = doc.uri.toString();
  clearTimer(key);
  const delay = config().get<number>("diagnostics.debounce", 350);
  timers.set(
    key,
    setTimeout(() => {
      timers.delete(key);
      check(doc);
    }, delay)
  );
}

function clearTimer(key: string): void {
  const t = timers.get(key);
  if (t) {
    clearTimeout(t);
    timers.delete(key);
  }
}

/**
 * Run `la3 check` against the document's current (possibly unsaved) text and
 * publish the resulting diagnostics. Unsaved buffers are checked via a temp
 * file so squiggles stay live before you hit save.
 */
function check(doc: vscode.TextDocument): void {
  const bin = resolveBinary();
  if (!bin) {
    promptMissingBinary();
    return;
  }

  let target = doc.uri.fsPath;
  let temp: string | null = null;
  if (doc.isDirty || doc.isUntitled) {
    temp = path.join(
      os.tmpdir(),
      `la3-check-${Buffer.from(doc.uri.toString()).toString("hex").slice(0, 24)}.la3`
    );
    try {
      fs.writeFileSync(temp, doc.getText());
    } catch (err) {
      output.appendLine(`could not write temp file: ${err}`);
      return;
    }
    target = temp;
  }

  cp.execFile(
    bin,
    ["check", target],
    { timeout: 10000 },
    (error: cp.ExecFileException | null, _stdout: string, stderr: string) => {
      if (temp) fs.unlink(temp, () => {});

      // `la3 check` exits 0 with no diagnostics; non-zero with diagnostics on
      // stderr. A spawn failure (ENOENT etc.) has no stderr to parse.
      if (error && error.code === "ENOENT") {
        promptMissingBinary();
        return;
      }
      diagnostics.set(doc.uri, parseDiagnostics(stderr || "", doc));
    }
  );
}

/** Turn `la3 check` stderr into VS Code diagnostics. */
function parseDiagnostics(
  stderr: string,
  doc: vscode.TextDocument
): vscode.Diagnostic[] {
  const out: vscode.Diagnostic[] = [];
  for (const raw of stderr.split("\n")) {
    const m = DIAG_RE.exec(raw.trimEnd());
    if (!m) continue;
    const [, phase, message, lineStr, colStr] = m;
    const line = Math.max(0, parseInt(lineStr, 10) - 1);
    const col = Math.max(0, parseInt(colStr, 10) - 1);

    // The compiler gives a single point; widen it to the token under the caret
    // so the squiggle is visible rather than a zero-width range.
    const range = tokenRange(doc, line, col);
    const d = new vscode.Diagnostic(range, message, vscode.DiagnosticSeverity.Error);
    d.source = "la3";
    d.code = phase;
    out.push(d);
  }
  return out;
}

/**
 * Build a range covering the identifier-like token starting at (line, col).
 * Falls back to a single column when the position is past the document.
 */
function tokenRange(
  doc: vscode.TextDocument,
  line: number,
  col: number
): vscode.Range {
  if (line < doc.lineCount) {
    const text = doc.lineAt(line).text;
    let end = col;
    while (end < text.length && /[A-Za-z0-9_]/.test(text[end])) end++;
    if (end === col) end = Math.min(text.length, col + 1);
    return new vscode.Range(line, col, line, end);
  }
  return new vscode.Range(line, col, line, col + 1);
}

/** Resolve the `la3` binary path; returns "la3" to defer to PATH as a last resort. */
function resolveBinary(): string {
  const configured = config().get<string>("path", "");
  if (configured) return configured;

  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    for (const rel of ["target/release/la3", "target/debug/la3"]) {
      const p = path.join(folder.uri.fsPath, rel);
      if (fs.existsSync(p)) return p;
    }
  }
  // Fall back to PATH; execFile will surface ENOENT if it is not there.
  return "la3";
}

function promptMissingBinary(): void {
  vscode.window
    .showWarningMessage(
      "La3: interpreter not found. Build it or set `la3.path`.",
      "Build now",
      "Open settings"
    )
    .then((choice) => {
      if (choice === "Build now") buildInterpreter();
      else if (choice === "Open settings")
        vscode.commands.executeCommand("workbench.action.openSettings", "la3.path");
    });
}

/** Find the cargo project root (the folder containing Cargo.toml). */
function cargoRoot(): string | undefined {
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    if (fs.existsSync(path.join(folder.uri.fsPath, "Cargo.toml")))
      return folder.uri.fsPath;
  }
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

/** Run `cargo build --release` in a terminal so the user sees progress. */
function buildInterpreter(): void {
  const cwd = cargoRoot();
  if (!cwd) {
    vscode.window.showErrorMessage("La3: no workspace folder with a Cargo.toml.");
    return;
  }
  const term = vscode.window.createTerminal({ name: "la3 build", cwd });
  term.show();
  term.sendText("cargo build --release");
}

/** Run the active file with `la3 run` in a terminal. */
function runActiveFile(): void {
  const doc = vscode.window.activeTextEditor?.document;
  if (!doc || doc.languageId !== "la3") {
    vscode.window.showErrorMessage("La3: no active .la3 file to run.");
    return;
  }
  const bin = resolveBinary();
  const save = doc.isDirty ? doc.save() : Promise.resolve(true);
  Promise.resolve(save).then(() => {
    const term = vscode.window.createTerminal({ name: "la3 run" });
    term.show();
    term.sendText(`${quote(bin)} run ${quote(doc.uri.fsPath)}`);
  });
}

function quote(s: string): string {
  return /[^A-Za-z0-9_./-]/.test(s) ? `"${s.replace(/"/g, '\\"')}"` : s;
}

function config(): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration("la3");
}

// Copy the freshly built release interpreter into the extension's platform
// specific bundle directory (bin/<platform>-<arch>/). Run after building with
// `cargo build --release`; the `bundle:bin` npm script does both.

import { mkdirSync, copyFileSync, chmodSync } from "node:fs";
import { join } from "node:path";

const exe = process.platform === "win32" ? "la3.exe" : "la3";
const dir = join("bin", `${process.platform}-${process.arch}`);
const dest = join(dir, exe);
const src = join("..", "..", "target", "release", exe);

mkdirSync(dir, { recursive: true });
copyFileSync(src, dest);
if (process.platform !== "win32") chmodSync(dest, 0o755);
console.log(`bundled ${src} -> ${dest}`);

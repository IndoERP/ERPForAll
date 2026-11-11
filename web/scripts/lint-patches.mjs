/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Ordered manifest: [{"addon":"my_addon","file":"./register.ts"}, ...]. Paths relative to manifest.
import { openSync, readSync, closeSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { API } from "typescript/unstable/sync";
import { lintPatchConflicts } from "../tooling/patch-lint.ts";

try {
  if (process.argv.length !== 4) throw new Error("usage: node web/scripts/lint-patches.mjs <tsconfig.json> <ordered-manifest.json>");
  const config = resolve(process.argv[2]); const manifest = resolve(process.argv[3]);
  const fd = openSync(manifest, "r");
  const raw = Buffer.alloc(1_048_577);
  let length = 0;
  try {
    while (length < raw.byteLength) {
      const count = readSync(fd, raw, length, raw.byteLength - length, null);
      if (count === 0) break;
      length += count;
    }
  } finally { closeSync(fd); }
  if (length > 1_048_576) throw new Error("Patch manifest exceeds 1048576 bytes");
  let text;
  try { text = new TextDecoder("utf-8", { fatal: true }).decode(raw.subarray(0, length)); }
  catch (cause) { throw new Error("Patch manifest must be valid UTF-8", { cause }); }
  let input;
  try { input = JSON.parse(text); }
  catch (cause) { throw new Error("Patch manifest must be valid JSON", { cause }); }
  if (!Array.isArray(input) || input.length > 4096) throw new Error("Patch manifest must be an array of at most 4096 entries");
  const ids = new Set(); const files = new Set();
  const entries = input.map(entry => {
    if (entry === null || typeof entry !== "object" || Object.keys(entry).sort().join(",") !== "addon,file" ||
        typeof entry.addon !== "string" || entry.addon.length === 0 || entry.addon.length > 256 ||
        typeof entry.file !== "string" || entry.file.length === 0 || entry.file.length > 4096) {
      throw new Error("Patch manifest entries require only addon (1..256 code units) and file (1..4096 code units)");
    }
    const file = resolve(dirname(manifest), entry.file);
    if (ids.has(entry.addon) || files.has(file)) throw new Error("Patch manifest repeats an addon or source file");
    ids.add(entry.addon); files.add(file);
    return { addon: entry.addon, file };
  });
  const api = new API({ cwd: dirname(config) });
  try {
    const snapshot = api.updateSnapshot({ openProjects: [config] });
    try {
      const project = snapshot.getProject(config);
      if (project === undefined) throw new Error(`Cannot load patch lint project: ${config}`);
      const diagnostics = lintPatchConflicts(project, entries);
      process.stdout.write(JSON.stringify({ entries: entries.length, diagnostics }) + "\n");
      if (diagnostics.length > 0) process.exitCode = 1;
    } finally { snapshot.dispose(); }
  } finally { api.close(); }
} catch (error) {
  console.error(`patch-lint: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 2;
}

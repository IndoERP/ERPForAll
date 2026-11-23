/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// List the client texts an addon passes to `t()` (addons/web/static/src/client/translate.ts), as the skeleton of a
// translation catalog: every English source with an empty translation, sorted.
//
//   node scripts/extract-client-strings.mjs [addon]      # default: web
//
// A `t(` call whose first argument is not one plain string literal cannot be extracted and is refused with its place,
// so every translatable text stays listable.

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

// `t(` as a call of its own (not `.t(`, `set(`, `split(`), then a double-quoted literal followed by `,` or `)`.
const CALL = /(?<![\w$.])t\(/g;
const LITERAL = /^"((?:[^"\\\n]|\\.)*)"\s*[,)]/;

/** The English sources of `t()` calls in `text`; `file` names the place of a refusal. */
export function extractClientStrings(text, file = "<source>") {
  const found = [];
  for (const match of text.matchAll(CALL)) {
    const start = match.index + match[0].length;
    const literal = LITERAL.exec(text.slice(start));
    if (!literal) {
      const line = text.slice(0, match.index).split("\n").length;
      throw new Error(`${file}:${line}: t() takes one plain string literal as its text, so the text can be extracted.`);
    }
    found.push(JSON.parse(`"${literal[1]}"`));
  }
  return found;
}

function sources(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) { if (name !== "vendor" && name !== "node_modules") sources(path, out); }
    else if (name.endsWith(".ts") && !name.endsWith(".d.ts")) out.push(path);
  }
  return out;
}

/** Every text of `addon`'s client sources, as a catalog skeleton (source to empty translation, sorted). */
export function clientCatalogSkeleton(addon = "web") {
  if (!/^[a-z][a-z0-9_]*$/.test(addon)) throw new Error(`Invalid addon name: ${addon}.`);
  const root = join(repo, "addons", addon, "static", "src");
  if (!existsSync(root)) throw new Error(`The addon ${addon} has no client sources (addons/${addon}/static/src).`);
  const texts = new Set();
  for (const file of sources(root)) {
    // translate.ts documents the call in its own comments; it declares no text.
    if (file.endsWith(join("client", "translate.ts"))) continue;
    for (const text of extractClientStrings(readFileSync(file, "utf8"), relative(repo, file))) texts.add(text);
  }
  return Object.fromEntries([...texts].sort().map(text => [text, ""]));
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.stdout.write(`${JSON.stringify(clientCatalogSkeleton(process.argv[2]), null, 2)}\n`);
}

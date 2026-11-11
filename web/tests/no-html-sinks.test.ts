/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Source scan: no runtime or vendored source assigns HTML or runs a
// string as code, except the two named places in src/runtime/dom.ts (template's <template> element and
// a Markup value in insert/out).

import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

const web = join(dirname(fileURLToPath(import.meta.url)), "..");
const SINK = /innerHTML|outerHTML|insertAdjacentHTML|document\.write|\beval\s*\(|new\s+Function\b|\bset(?:Timeout|Interval)\s*\(\s*["'`]|createContextualFragment|DOMParser/;
const ALLOWED = new Map([
  ["src/runtime/dom.ts", ["t.innerHTML = html;", "t.innerHTML = h;"]],
]);

function sources(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) sources(p, out);
    else if (/\.(ts|js|mjs)$/.test(name) && !name.endsWith(".d.ts")) out.push(p);
  }
  return out;
}

/** The code with comments blanked (line structure kept), so a sink named in a comment is not a hit. */
function code(text: string): string {
  return text.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g, (c) => c.replace(/[^\n]/g, " "));
}

test("no HTML sink or string evaluation outside the two named places", () => {
  const hits: string[] = [];
  const allowedSeen: string[] = [];
  for (const file of [...sources(join(web, "src")), ...sources(join(web, "vendor"))]) {
    const rel = relative(web, file).split("\\").join("/");
    const lines = code(readFileSync(file, "utf8")).split("\n");
    lines.forEach((line, i) => {
      if (!SINK.test(line)) return;
      const allowed = ALLOWED.get(rel)?.find((a) => line.trim() === a);
      if (allowed !== undefined) allowedSeen.push(`${rel}: ${allowed}`);
      else hits.push(`${rel}:${i + 1}: ${line.trim()}`);
    });
  }
  expect(hits).toEqual([]);
  expect(allowedSeen.sort()).toEqual(["src/runtime/dom.ts: t.innerHTML = h;", "src/runtime/dom.ts: t.innerHTML = html;"]);
});

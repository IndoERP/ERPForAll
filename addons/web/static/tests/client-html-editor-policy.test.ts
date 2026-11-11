/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/// <reference types="node" />
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";

const repo = resolve(process.cwd(), "..");
function list(source: string, name: string): readonly string[] {
  const declaration = source.match(new RegExp(`(?:static|const) ${name}: [^=]+?= &\\[([\\s\\S]*?)\\];`));
  if (!declaration) throw new Error(`Missing server HTML policy declaration: ${name}.`);
  const values = [...declaration[1]!.matchAll(/"([a-z0-9-]+)"/g)].map(match => match[1]!);
  if (!values.length) throw new Error(`Empty server HTML policy declaration: ${name}.`);
  return values;
}
// C1: the actual client vocabulary is checked against erp-html's current default-policy
// tables (crates/erp-html/src/tables.rs ALLOW_TAGS/KILL_TAGS/REMOVE_TAGS) and clean.rs FORM_KILL/judge.
// This source contract does not claim byte-for-byte sanitizer output or field-write integration.
test("client HTML vocabulary is an unchanged-element subset of the default server policy", () => {
  const client = readFileSync(resolve(repo, "addons/web/static/src/client/html-editor-document.ts"), "utf8");
  const declaration = client.match(/const vocabulary = new Set\(\[([^\]]+)\]\)/);
  expect(declaration, "The actual client vocabulary declaration must remain discoverable").not.toBeNull();
  const names = [...declaration![1]!.matchAll(/"([a-z0-9]+)"/g)].map(match => match[1]!);
  expect(names.length).toBeGreaterThan(0); expect(new Set(names).size).toBe(names.length);
  const tables = readFileSync(resolve(repo, "crates/erp-html/src/tables.rs"), "utf8");
  const cleaner = readFileSync(resolve(repo, "crates/erp-html/src/clean.rs"), "utf8");
  const allowed = new Set(list(tables, "ALLOW_TAGS"));
  const removed = new Set([...list(tables, "KILL_TAGS"), ...list(tables, "REMOVE_TAGS"), ...list(cleaner, "FORM_KILL"), "form"]);
  expect(names.filter(name => !allowed.has(name) || removed.has(name))).toEqual([]);
  // Pin the current rule as well as complete semantic source identities below; membership alone
  // would miss a changed lookup implementation or a removal condition in the caller.
  const judge = cleaner.slice(cleaner.indexOf("fn judge("), cleaner.indexOf("/// Whether an element has"));
  expect(judge.replace(/\s+/g, " ").trim()).toBe('fn judge(local: &str) -> Verdict { let name = lower(local); if tables::is_kill_tag(&name) || FORM_KILL.contains(&&*name) { Verdict::Kill } else if tables::is_remove_tag(&name) || &*name == "form" || !tables::is_allowed_tag(&name) { Verdict::Unwrap } else { Verdict::Keep } }');
});


const identity = (sources: Readonly<Record<string, string>>, read: (name: string) => string): string[] => Object.entries(sources).filter(([name, expected]) => createHash("sha256").update(read(name)).digest("hex") !== expected).map(([name]) => name);
test("server policy lookup and caller semantic changes invalidate the reviewed source projection", () => {
  const fixture = JSON.parse(readFileSync(resolve(repo, "addons/web/static/tests/html-editor-server-policy.json"), "utf8")) as {sources: Record<string, string>};
  const read = (name: string): string => readFileSync(resolve(repo, name), "utf8");
  expect(Object.keys(fixture.sources).sort()).toEqual(["crates/erp-html/src/clean.rs", "crates/erp-html/src/normalize.rs", "crates/erp-html/src/policy.rs", "crates/erp-html/src/serialize.rs", "crates/erp-html/src/tables.rs"]);
  expect(identity(fixture.sources, read)).toEqual([]);
  // Exercise the same identity function used above with two concrete semantic mutations.
  const lookup = "crates/erp-html/src/tables.rs", caller = "crates/erp-html/src/clean.rs";
  const changedLookup = read(lookup).replace("ALLOW_TAGS.binary_search(&name).is_ok()", 'name != "strong" && ALLOW_TAGS.binary_search(&name).is_ok()');
  expect(changedLookup).not.toBe(read(lookup));
  expect(identity(fixture.sources, name => name === lookup ? changedLookup : read(name))).toEqual([lookup]);
  const refusedStrong = read(caller).replace("fn judge(local: &str) -> Verdict {", 'fn judge(local: &str) -> Verdict { if local == "strong" { return Verdict::Kill; }');
  expect(refusedStrong).not.toBe(read(caller));
  expect(identity(fixture.sources, name => name === caller ? refusedStrong : read(name))).toEqual([caller]);
});

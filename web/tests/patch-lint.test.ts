/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { API } from "typescript/unstable/sync";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { lintPatchConflicts } from "../tooling/patch-lint.js";

function lint(a: string, b: string,
  targetSource = "export class Target { method(n: number): number { return n; } static method(n: number) { return n; } }",
  imports: readonly [string, string] = ["Target", "Target as Alias"]) {
  const files = new Map([
    ["/target.ts", targetSource],
    ["/a.ts", `import { ${imports[0]} } from './target.js'; export function register(ctx: any) { ${a} }`],
    ["/b.ts", `import { ${imports[1]} } from './target.js'; export function register(ctx: any) { ${b} }`],
  ]);
  const dir = mkdtempSync(join(tmpdir(), "ui4-lint-"));
  const api = new API({ cwd: dir });
  try {
    for (const [name, source] of files) writeFileSync(join(dir, name.slice(1)), source);
    const config = join(dir, "tsconfig.json");
    writeFileSync(config, JSON.stringify({ compilerOptions: { noLib: true, module: "nodenext" }, include: ["*.ts"] }));
    const snapshot = api.updateSnapshot({ openProjects: [config] });
    try {
      const project = snapshot.getProject(config)!;
      return lintPatchConflicts(project, [{ addon: "a", file: join(dir, "a.ts") }, { addon: "b", file: join(dir, "b.ts") }])
        .map(d => ({ ...d, file: "/" + d.file.split("/").pop() }));
    } finally { snapshot.dispose(); }
  } finally { api.close(); rmSync(dir, { recursive: true, force: true }); }
}

/// (partial: import aliases and cross-addon destructive replacement)
test("lint resolves target import aliases and flags cross-addon destructive replacement", () => {
  const result = lint("ctx.patch(Target, sup => ({ method(n: number) { return sup.method.call(this, n) + 1; } }));",
    "ctx.patch(Alias, sup => ({ method(n: number) { return n * 2; } }));");
  expect(result).toHaveLength(1);
  expect(result[0]).toMatchObject({ code: "patch-conflict", addon: "b", previousAddon: "a", member: "method", file: "/b.ts" });
  expect(result[0]!.message).toBe('Addon "b" replaces "method" from "a" without calling sup');
});

test("predecessor calls and distinct static targets do not conflict", () => {
  expect(lint("ctx.patch(Target, sup => ({ method(n: number) { return n; } }));",
    "ctx.patch(Alias, prev => ({ method(n: number) { return prev.method.call(this, n); } }));")).toEqual([]);
  expect(lint("ctx.patch(Target, sup => ({ method(n: number) { return n; } }));",
    "ctx.patchStatic(Alias, sup => ({ method(n: number) { return n; } }));")).toEqual([]);
  expect(lint("ctx.patch(Target.prototype, () => ({ method(n: number) { return n; } }));",
    "ctx.patch(Alias, () => ({ method(n: number) { return n; } }));")).toMatchObject([
      { code: "patch-conflict", message: 'Addon "b" replaces "method" from "a" without calling sup' },
    ]);
  expect(lint("ctx.patchStatic(Target, () => ({ method(n: number) { return n; } }));",
    "ctx.patch(Alias.prototype, () => ({ method(n: number) { return n; } }));")).toEqual([]);
});

test("a string or comment mentioning sup and calling a different member do not hide conflicts", () => {
  const result = lint("ctx.patch(Target, () => ({ method(n: number) { return n; } }));",
    "ctx.patch(Alias, sup => ({ method(n: number) { /* sup.method() */ const text = 'sup.method()'; sup.other(); return n; } }));");
  expect(result).toMatchObject([{ code: "patch-conflict", message: 'Addon "b" replaces "method" from "a" without calling sup' }]);
});

test("dynamic factories, targets and members are diagnosed instead of silently skipped", () => {
  for (const [source, message] of [
    ["ctx.patch(getTarget(), makeFactory);", "Dynamic patch targets are unanalysable"],
    ["ctx.patch(Alias, makeFactory);", "Patch factory must be an inline function with an optional named predecessor"],
    ["ctx.patch(Alias, sup => ({ [name]() { return 1; } }));", "Dynamic patch members and spreads are unanalysable"],
    ["ctx['patch'](Alias, () => ({ method() { return 1; } }));", "Computed patch capabilities are unanalysable; use direct ctx.patch/patchStatic"],
    ["const {patch: p} = ctx; p(Alias, () => ({}));", "Destructured patch capabilities are unanalysable; use direct ctx.patch/patchStatic"],
    ["ctx[capability](Alias, () => ({}));", "Computed patch capabilities are unanalysable; use direct ctx.patch/patchStatic"],
    ["const other = ctx; other.patch(Alias, () => ({}));", "Patch calls must be direct statements in register(ctx); dynamic execution is unanalysable"],
  ] as const) {
    expect(lint("", source)).toMatchObject([{ code: "patch-unanalysable", message }]);
  }
});

test("quoted and computed destructured capabilities fail loudly (UI4-R1)", () => {
  for (const source of [
    "const {'patch': p} = ctx; p(Alias, () => ({}));",
    "const {'patchStatic': p} = ctx; p(Alias, () => ({}));",
    "const {['patch']: p} = ctx; p(Alias, () => ({}));",
    "const capability = 'patch'; const {[capability]: p} = ctx; p(Alias, () => ({}));",
  ]) {
    expect(lint("", source)).toMatchObject([{ code: "patch-unanalysable",
      message: "Destructured patch capabilities are unanalysable; use direct ctx.patch/patchStatic" }]);
  }
});

test("a plain object's prototype property and its imported alias share target identity (UI4-R2)", () => {
  const result = lint("ctx.patch(holder.prototype, () => ({ method(n: number) { return n; } }));",
    "ctx.patch(target, () => ({ method(n: number) { return n + 1; } }));",
    "export const holder = { prototype: { method(n: number) { return n; } } }; export const target = holder.prototype;",
    ["holder", "target"]);
  expect(result).toMatchObject([{ code: "patch-conflict", addon: "b", previousAddon: "a", member: "method",
    message: 'Addon "b" replaces "method" from "a" without calling sup' }]);
});

test("a never-invoked nested callback does not suppress a replacement diagnostic", () => {
  expect(lint("ctx.patch(Target, () => ({ method(n: number) { return n; } }));",
    "ctx.patch(Alias, sup => ({ method(n: number) { const unused = () => sup.method(n); return n; } }));")
    ).toMatchObject([{ code: "patch-conflict", message: 'Addon "b" replaces "method" from "a" without calling sup' }]);
});

test("getters and setters can delegate through Reflect with the live receiver", () => {
  expect(lint("ctx.patch(Target, () => ({ get value() { return 1; } }));",
    "ctx.patch(Alias, sup => ({ get value() { return Reflect.get(sup, 'value', this); } }));")).toEqual([]);
  expect(lint("ctx.patch(Target, () => ({ set value(n: number) {} }));",
    "ctx.patch(Alias, sup => { return { set value(n: number) { Reflect.set(sup, 'value', n, this); } }; });")).toEqual([]);
});

// The TypeScript native API server may print one "context canceled" line on stderr when the CLI closes it
// after an error; only that exact line is dropped, every other stderr line is still compared.
const errorText = (result: { stderr: string }) =>
  result.stderr.split("\n").filter(line => line.trim() !== "context canceled").join("\n").trim();

test("lint CLI reports conflicts, clean manifests and malformed or missing inputs", () => {
  const dir = mkdtempSync(join(tmpdir(), "ui4-cli-"));
  try {
    const script = join(import.meta.dirname, "../scripts/lint-patches.mjs");
    const config = join(dir, "tsconfig.json"); const manifest = join(dir, "patches.json");
    writeFileSync(config, JSON.stringify({ compilerOptions: { noLib: true, module: "nodenext" }, include: ["*.ts"] }));
    writeFileSync(join(dir, "target.ts"), "export const target = { n: 1 }; export class Static { static n = 1; }");
    for (const id of ["a", "b"]) writeFileSync(join(dir, `${id}.ts`),
      "import {target} from './target.js'; export function register(ctx: any) { ctx.patch(target, () => ({ n: 2 })); }");
    const run = () => spawnSync(process.execPath, [script, config, manifest], { encoding: "utf8", timeout: 10_000 });
    writeFileSync(manifest, JSON.stringify([{ addon: "a", file: "a.ts" }, { addon: "b", file: "b.ts" }]));
    const conflict = run(); expect(conflict.status).toBe(1);
    expect(JSON.parse(conflict.stdout).diagnostics[0]).toMatchObject({ code: "patch-conflict", addon: "b", previousAddon: "a", member: "n",
      message: 'Addon "b" replaces "n" from "a" without calling sup' });
    for (const source of [
      "const {'patch': p} = ctx; p(target, () => ({ n: 2 }));",
      "const {'patchStatic': p} = ctx; p(Static, () => ({ n: 2 }));",
      "const {['patch']: p} = ctx; p(target, () => ({ n: 2 }));",
      "const capability = 'patch'; const {[capability]: p} = ctx; p(target, () => ({ n: 2 }));",
    ]) {
      writeFileSync(join(dir, "b.ts"), `import {target, Static} from './target.js'; export function register(ctx: any) { ${source} }`);
      const aliased = run(); expect(aliased.status).toBe(1);
      expect(JSON.parse(aliased.stdout).diagnostics).toMatchObject([{ code: "patch-unanalysable",
        message: "Destructured patch capabilities are unanalysable; use direct ctx.patch/patchStatic" }]);
    }
    writeFileSync(manifest, "[]"); const empty = run(); expect(empty.status).toBe(0);
    expect(JSON.parse(empty.stdout)).toEqual({ entries: 0, diagnostics: [] });
    writeFileSync(manifest, '[{"addon":"a","file":"missing.ts"}]'); const missing = run();
    expect(missing.status).toBe(2); expect(errorText(missing)).toBe(`patch-lint: Patch lint source is not in the project: ${join(dir, "missing.ts")}`);
    writeFileSync(manifest, '[{"addon":"a","file":"a.ts","typo":true}]'); const malformed = run();
    expect(malformed.status).toBe(2); expect(errorText(malformed)).toBe("patch-lint: Patch manifest entries require only addon (1..256 code units) and file (1..4096 code units)");
    writeFileSync(manifest, '[{"addon":"a","file":"a.ts"},{"addon":"a","file":"b.ts"}]'); const duplicate = run();
    expect(duplicate.status).toBe(2); expect(errorText(duplicate)).toBe("patch-lint: Patch manifest repeats an addon or source file");
    writeFileSync(manifest, " ".repeat(1_048_577)); const oversized = run();
    expect(oversized.status).toBe(2); expect(errorText(oversized)).toBe("patch-lint: Patch manifest exceeds 1048576 bytes");
    writeFileSync(manifest, Buffer.concat([Buffer.from('[{"addon":"'), Buffer.from([0xff]), Buffer.from('","file":"a.ts"}]')]));
    const encoding = run(); expect(encoding.status).toBe(2); expect(errorText(encoding)).toBe("patch-lint: Patch manifest must be valid UTF-8");
    writeFileSync(manifest, '{"private_text": "do not print this"'); const invalidJson = run();
    expect(invalidJson.status).toBe(2); expect(errorText(invalidJson)).toBe("patch-lint: Patch manifest must be valid JSON");
  } finally { rmSync(dir, { recursive: true, force: true }); }
// The CLI spawns node and starts the TypeScript API server; on a loaded build host that exceeds the
// 10 s file default (merge CI 2026-10-31 at aed60fac2 timed out at 10 s with 2364 other tests green).
}, 60_000);

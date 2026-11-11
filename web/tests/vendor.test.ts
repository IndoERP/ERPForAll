/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The vendoring check: UPSTREAM.toml parsing and check-vendor.mjs
// against local trees (`--source`, no network; the CI step checks the real trees against GitHub).

import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, test } from "vitest";
import { parseUpstream } from "../scripts/upstream-toml.mjs";

const web = join(dirname(fileURLToPath(import.meta.url)), "..");
const COMMIT = "0123456789abcdef0123456789abcdef01234567";
const PRISTINE = "export const answer = 41;\n";
const sha = (s: string) => createHash("sha256").update(s).digest("hex");

const MANIFEST = (extra = "", patches = '["patches/0001-fix.patch"]') => `repository = "https://github.com/example/lib"
tag = "v1.0.0"
commit = "${COMMIT}"
license = "MIT"
patches = ${patches}
npm_package = "example-lib"
${extra}
[[files]]
path = "src/lib.js"
upstream = "src/lib.js"
sha256 = "${sha(PRISTINE)}"
`;

const temps: string[] = [];
afterEach(() => {
  for (const t of temps.splice(0)) rmSync(t, { recursive: true, force: true });
});

/** A vendor dir with one tree `lib` (pristine file + one patch) and a matching --source dir. */
function fixture(): { vendor: string; source: string; tree: string } {
  const root = mkdtempSync(join(tmpdir(), "vendor-test-"));
  temps.push(root);
  const source = join(root, "source", "github.com", "example", "lib", COMMIT, "src");
  mkdirSync(source, { recursive: true });
  writeFileSync(join(source, "lib.js"), PRISTINE);
  const tree = join(root, "vendor", "lib");
  mkdirSync(join(tree, "src"), { recursive: true });
  mkdirSync(join(tree, "patches"));
  writeFileSync(
    join(tree, "patches", "0001-fix.patch"),
    "--- a/src/lib.js\n+++ b/src/lib.js\n@@ -1 +1 @@\n-export const answer = 41;\n+export const answer = 42;\n",
  );
  writeFileSync(join(tree, "src", "lib.js"), "export const answer = 42;\n");
  writeFileSync(join(tree, "UPSTREAM.toml"), MANIFEST());
  return { vendor: join(root, "vendor"), source: join(root, "source"), tree };
}

function run(vendor: string, source: string): { status: number | null; out: string } {
  const r = spawnSync(process.execPath, [join(web, "scripts/check-vendor.mjs"), "--vendor", vendor, "--source", source], { encoding: "utf8" });
  return { status: r.status, out: `${r.stdout}${r.stderr}` };
}

test("UPSTREAM.toml: unknown, duplicate and missing keys, bad hashes and half-pinned files are errors", () => {
  expect(parseUpstream(MANIFEST(), "m").files).toHaveLength(1);
  expect(parseUpstream(MANIFEST('local = ["src/x.d.ts"]'), "m").local).toEqual(["src/x.d.ts"]);
  expect(() => parseUpstream(MANIFEST('flavour = "x"'), "m")).toThrow(/unknown key 'flavour'/);
  expect(() => parseUpstream(MANIFEST('tag = "v2"'), "m")).toThrow(/duplicate key 'tag'/);
  expect(() => parseUpstream(MANIFEST().replace(/^license = .*\n/m, ""), "m")).toThrow(/missing key 'license'/);
  expect(() => parseUpstream(MANIFEST().replace(/sha256 = "[0-9a-f]+"/, 'sha256 = "abc"'), "m")).toThrow(/64 hex/);
  expect(() => parseUpstream(`${MANIFEST()}repository = "https://github.com/x/y"\n`, "m")).toThrow(/both 'repository' and 'commit'/);
  expect(() => parseUpstream(MANIFEST().replace("[[files]]", "[files]"), "m")).toThrow(/not a key = value line/);
});

test("check-vendor accepts a tree equal to upstream plus its patches", () => {
  const { vendor, source } = fixture();
  const r = run(vendor, source);
  expect(r.out).toContain("lib v1.0.0");
  expect(r.status).toBe(0);
});

test("check-vendor fails on an edited file, an unlisted file, a missing patch and a wrong upstream hash", () => {
  let f = fixture();
  writeFileSync(join(f.tree, "src", "lib.js"), "export const answer = 43;\n");
  expect(run(f.vendor, f.source)).toMatchObject({ status: 1 });
  expect(run(f.vendor, f.source).out).toContain("differs from upstream v1.0.0 plus the listed patches");

  f = fixture();
  writeFileSync(join(f.tree, "src", "extra.js"), "x");
  expect(run(f.vendor, f.source).out).toContain("src/extra.js is not listed in UPSTREAM.toml");

  f = fixture();
  writeFileSync(join(f.tree, "UPSTREAM.toml"), MANIFEST("", "[]"));
  const noPatch = run(f.vendor, f.source);
  expect(noPatch.status).toBe(1);
  expect(noPatch.out).toMatch(/patches\/0001-fix\.patch is not listed|differs from upstream/);

  f = fixture();
  writeFileSync(join(f.source, "github.com/example/lib", COMMIT, "src/lib.js"), "changed upstream\n");
  expect(run(f.vendor, f.source).out).toContain("upstream SHA-256 is");
});

test("the real vendored trees are listed with their legal headers", () => {
  for (const [tree, header] of [
    ["alien-signals/src/index.ts", "alien-signals 3.2.1"],
    ["alien-signals/src/system.ts", "alien-signals 3.2.1"],
    ["dom-expressions/src/reconcile.js", "dom-expressions 0.40.12"],
  ] as const) {
    expect(readFileSync(join(web, "vendor", tree), "utf8").slice(0, 200)).toContain(header);
  }
  expect(execFileSync("git", ["--version"], { encoding: "utf8" })).toMatch(/^git version/);
});

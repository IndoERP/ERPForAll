/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Proves the mechanism behind typechecks/abi-emitted-conformance.ts: a small,
// self-contained isolated TypeScript project, using the real `Exact`/`Same` utilities from
// typechecks/type-identity.ts (read from disk, never copied by hand, so this test tracks the real
// mechanism rather than a parallel reimplementation of it), that compares a tiny "frozen contract"
// against a tiny "emitted declaration". Each case below mutates the emitted side one way and asserts
// `tsc` reports a diagnostic; the matching baseline must stay clean. The real check
// (tsconfig.dts-consumer.json) runs the same two type aliases against the real abi.d.ts and the real
// `node_modules/.elang-dts/src/runtime/abi-impl.d.ts`; this suite cannot depend on that emission having run
// (vitest can execute standalone, before `pnpm exec tsc -p tsconfig.dts.json`), so it exercises the
// identical mechanism against small, self-contained fixtures instead.
//
// Deliberately shells out to the real `tsc` binary (`node_modules/.bin/tsc`, one isolated project per
// case, a fresh temp directory each time) rather than importing the `typescript` package's compiler API
// as a JS module: that import broke under this workspace's `moduleResolution: "bundler"` (confirmed on
// CI — both a default and a namespace import of `"typescript"` resolve, for type-checking purposes, to
// the package's internal `lib/version` submodule instead of its compiler API, which is a package/module
// resolution quirk this project has never needed to work around because nothing else in this repository
// ever imports `typescript` as a module; every other check runs the CLI, as this one now does too).

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const TSC_BIN = join(here, "../node_modules/.bin/tsc");
const TYPE_IDENTITY_SOURCE = readFileSync(join(here, "../typechecks/type-identity.ts"), "utf8");

const FROZEN = `
export declare function alpha(x: string): void;
export declare function beta(x: number, y: number): string;
`;

const BASELINE_EMITTED = `
export declare function alpha(x: string): void;
export declare function beta(x: number, y: number): string;
`;

/**
 * The real `typechecks/abi-emitted-conformance.ts`, with its two real import specifiers rewritten to
 * the fixture's local paths. Read from disk (never hand-copied) so a future structural change to the
 * real consumer — e.g. dropping `sameNames`, or renaming the pattern — is exercised by this test too,
 * instead of only by a parallel fixture that could silently drift from it.
 */
function consumerSource(): string {
  const real = readFileSync(join(here, "../typechecks/abi-emitted-conformance.ts"), "utf8");
  // Each replacement is checked individually (not just "the file changed somehow"): a real consumer
  // path edit that updates one specifier and leaves the other's search string stale would otherwise
  // silently no-op the second `.replace()` while the first still makes `rewritten !== real` true,
  // hiding the drift (exactly what happened here once, caught only by the resulting `tsc` run, not by
  // this guard — this guard now closes that gap).
  const rewrites: ReadonlyArray<readonly [string, string]> = [
    ['"../src/runtime/abi.js"', '"./frozen.js"'],
    ['"../node_modules/.elang-dts/src/runtime/abi-impl.js"', '"./emitted.js"'],
  ];
  let rewritten = real;
  for (const [search, replacement] of rewrites) {
    const occurrences = rewritten.split(search).length - 1;
    if (occurrences !== 1) {
      throw new Error(
        `consumerSource: expected import specifier ${search} to occur exactly once, found ${occurrences} — update this rewrite`,
      );
    }
    rewritten = rewritten.replace(search, replacement);
  }
  return rewritten;
}

const CONSUMER = consumerSource();

const tempDirs: string[] = [];
afterEach(() => {
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

/** Writes one isolated, self-contained `tsc` project (own tsconfig.json) to a fresh temp directory. */
function writeProject(files: Readonly<Record<string, string>>, skipLibCheck: boolean): string {
  const dir = mkdtempSync(join(tmpdir(), "abi-emitted-consumer-"));
  tempDirs.push(dir);
  for (const [name, text] of Object.entries(files)) writeFileSync(join(dir, name), text);
  // No `noLib`: this runs the real `tsc` binary against its own installed `lib.*.d.ts` files, so the
  // ordinary target-implied lib (`ES2022`) is as available here as in any real build — `noLib: true`
  // was tried first and failed (`TS2318: Cannot find global type 'Array'`/`'Object'`/… — TypeScript's
  // own type-checking machinery needs these even for code that only uses primitives), confirmed on the
  // sixth remote CI run and documented's revision log.
  // This is a deliberately minimal fixture config, not a mirror of the real `tsconfig.dts-consumer.json`
  // (it omits `verbatimModuleSyntax`, `isolatedModules`, `noUnusedLocals`/`Parameters`,
  // `noUncheckedIndexedAccess`): none of those flags affect the assertions below, since this suite
  // proves the `Exact`/`Same` mechanism in `type-identity.ts` (read from disk, §above), not the real
  // project's exact compiler-option set.
  const tsconfig = {
    compilerOptions: {
      strict: true,
      noEmit: true,
      types: [],
      skipLibCheck,
      target: "ES2022",
      module: "ESNext",
      moduleResolution: "bundler",
    },
    include: ["*.ts", "*.d.ts"],
  };
  writeFileSync(join(dir, "tsconfig.json"), JSON.stringify(tsconfig));
  return dir;
}

/**
 * Runs the isolated project; returns its `tsc` output and whether it compiled clean. A genuine spawn
 * failure (missing/unexecutable `tsc` binary, a crash, a signal — `execFileSync` throws without a
 * numeric `status`, since the process never exited normally) is rethrown rather than folded into
 * `{ ok: false }`: counting a broken toolchain as "the mutation was caught" would let every mutation
 * test pass vacuously for the wrong reason (silent-failure review finding).
 */
function runTsc(dir: string): { ok: boolean; output: string } {
  try {
    const output = execFileSync(TSC_BIN, ["-p", "tsconfig.json"], { cwd: dir, encoding: "utf8" });
    return { ok: true, output };
  } catch (error) {
    if (!(error instanceof Error) || typeof (error as NodeJS.ErrnoException & { status?: unknown }).status !== "number") {
      throw error;
    }
    const output = `${(error as { stdout?: string }).stdout ?? ""}${(error as { stderr?: string }).stderr ?? ""}`;
    return { ok: false, output };
  }
}

/** Builds and runs the isolated consumer project for one `emitted.d.ts` variant. */
function checkEmitted(emittedSource: string, skipLibCheck: boolean): { ok: boolean; output: string } {
  const dir = writeProject(
    {
      "frozen.d.ts": FROZEN,
      "emitted.d.ts": emittedSource,
      "type-identity.ts": TYPE_IDENTITY_SOURCE,
      "consumer.ts": CONSUMER,
    },
    skipLibCheck,
  );
  return runTsc(dir);
}

describe("the emitted-declaration consumer mechanism (type-identity.ts)", () => {
  test("a matching emission compiles clean", () => {
    const result = checkEmitted(BASELINE_EMITTED, false);
    expect(result.ok, result.output).toBe(true);
  });

  test("an unresolvable import is a loud diagnostic, not silence", () => {
    // Pins the harness's own soundness: if module resolution in these isolated projects ever broke
    // (e.g. stopped matching the `.js` → `.d.ts` fixture convention), every case below would see a
    // resolution error rather than silently reporting "compiles clean" for the wrong reason. Proven
    // directly, not only inferred: an import of a module absent from the project directory is a real
    // TS2307, surfaced in `tsc`'s own output.
    const dir = writeProject(
      { "consumer.ts": `import type * as Missing from "./does-not-exist.js";\nexport type T = Missing.Anything;\n` },
      false,
    );
    const result = runTsc(dir);
    expect(result.ok).toBe(false);
    expect(result.output).toContain("TS2307");
  });

  // Each of these four asserts the specific diagnostic (`TS2322`, confirmed on real CI output for all
  // four: `Exact`/`Same` resolving `false` fails the `const signatures`/`sameNames` assignment), not
  // only `result.ok === false`. Asserting `ok` alone would pass vacuously if `tsc` ever failed to run
  // to completion for an unrelated reason (silent-failure review finding) — `runTsc` already rethrows a
  // genuine spawn/crash failure rather than reporting it as `ok: false`, but asserting the diagnostic
  // content here is the second, independent layer: it proves *this* mechanism caught *this* mutation,
  // not merely that something, somewhere, went wrong.
  test("a removed export fails", () => {
    const result = checkEmitted(`export declare function alpha(x: string): void;`, false);
    expect(result.ok, result.output).toBe(false);
    expect(result.output).toContain("TS2322");
  });

  test("an added export fails", () => {
    const result = checkEmitted(`${BASELINE_EMITTED}\nexport declare function gamma(): void;`, false);
    expect(result.ok, result.output).toBe(false);
    expect(result.output).toContain("TS2322");
  });

  test("an added optional parameter fails", () => {
    const result = checkEmitted(
      `
export declare function alpha(x: string, extra?: number): void;
export declare function beta(x: number, y: number): string;
`,
      false,
    );
    expect(result.ok, result.output).toBe(false);
    expect(result.output).toContain("TS2322");
  });

  test("a changed return type fails", () => {
    const result = checkEmitted(
      `
export declare function alpha(x: string): void;
export declare function beta(x: number, y: number): number;
`,
      false,
    );
    expect(result.ok, result.output).toBe(false);
    expect(result.output).toContain("TS2322");
  });

  test("skipLibCheck: false catches an internally unresolved emitted declaration that skipLibCheck: true hides", () => {
    // Neither fixture declares `Opaque`: a generator bug could emit a reference to a type that no
    // longer exists. This checks skipLibCheck's effect in isolation, on the emitted file alone — no
    // Contract, no consumer — because routing it through `checkEmitted()` (as every other case here
    // does) would conflate two independent claims: an unresolvable reference also changes `alpha`'s
    // observable type to an error type, which the Same/Exact checks above would catch on their own
    // *regardless* of skipLibCheck (confirmed: that was this test's first, wrong version — it asserted
    // `skipLibCheck: true` compiles clean and was itself red, because `tsc` still flagged `alpha`
    // through Signatures, for an unrelated reason). skipLibCheck's own effect — hiding or surfacing an
    // internal defect in the .d.ts file itself — is a separate claim from the previous five cases, so
    // it needs its own fixture: exactly this file, with no consumer to blur the two claims together
    // ("generated declarations must not evade checking through the
    // workspace's existing library-skip setting").
    const malformed = `export declare function alpha(x: Opaque): void;\n`;
    const dir = (skipLibCheck: boolean) => writeProject({ "emitted.d.ts": malformed }, skipLibCheck);
    const hidden = runTsc(dir(true));
    const caught = runTsc(dir(false));
    expect(hidden.ok, hidden.output).toBe(true);
    expect(caught.ok, caught.output).toBe(false);
  });
});

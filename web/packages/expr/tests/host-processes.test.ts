/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
import { execFile } from "node:child_process";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { build } from "esbuild";
import { expect, test } from "vitest";

const execute = promisify(execFile);

// Functional process isolation, with no timing/allocation/performance acceptance threshold.
///
test("independent client processes and replacement preserve admission ceilings", async () => {
  const bundle = await build({
    stdin: {
      resolveDir: fileURLToPath(new URL("../src/", import.meta.url)),
      sourcefile: "host-process-admission.ts",
      loader: "ts",
      contents: `
        import { Budget } from "./budget";
        import { intVal } from "./values";
        const limits = { maxTotalBytes: 64, maxValueBytes: 128 };
        const budget = new Budget(limits);
        const value = budget.admit({ kind: "list", items: [intVal(1n)] });
        let error;
        try { new Budget(limits).admit({ kind: "list", items: [intVal(1n), intVal(2n)] }); }
        catch (e) { error = { kind: e.className, message: e.messageDetail }; }
        const next = new Budget(limits);
        next.admit(intVal(2n));
        console.log(JSON.stringify({ pid: process.pid, value: value.kind,
          bytes: budget.bytesUsed, steps: budget.stepsUsed, error, nextBytes: next.bytesUsed }));
      `,
    },
    bundle: true, platform: "node", format: "cjs", target: "es2023",
    write: false, logLevel: "silent", legalComments: "inline",
  });
  const code = bundle.outputFiles[0]?.text;
  if (code === undefined) throw new Error("admission process bundle is missing");
  async function run() {
    const result = await execute(process.execPath, ["-e", code!], {
      encoding: "utf8", timeout: 10_000, maxBuffer: 1024 * 1024,
    });
    expect(result.stderr).toBe("");
    return JSON.parse(result.stdout) as Record<string, unknown>;
  }
  const [a, b] = await Promise.all([run(), run()]);
  // The two application processes have exited before the replacement starts.
  const replacement = await run();
  expect(a["pid"]).not.toBe(b["pid"]);
  expect(replacement["pid"]).not.toBe(a["pid"]);
  for (const result of [a, b, replacement]) {
    expect(result).toMatchObject({ value: "list", bytes: 64, steps: 2, nextBytes: 32,
      error: { kind: "LimitExceeded", message: "evaluation memory budget of 64 bytes exceeded" } });
  }
});

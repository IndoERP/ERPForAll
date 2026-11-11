/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { afterEach, expect, test } from "vitest";
const run = promisify(execFile);
const roots: string[] = [];
afterEach(async () => { await Promise.all(roots.splice(0).map(root => rm(root, { recursive: true, force: true }))); });
async function fixture() {
  const root = await mkdtemp(path.join(tmpdir(), "browser-harness-contract-")); roots.push(root);
  for (const dir of ["web/scripts", "web/dist/human-ui", "crates/erp-server/brand"]) await mkdir(path.join(root, dir), { recursive: true });
  for (const name of ["browser-human-ui.mjs", "browser-readiness.mjs"]) {
    await copyFile(new URL(`../scripts/${name}`, import.meta.url), path.join(root, "web/scripts", name));
  }
  const assets = [];
  for (const [extension, content_type] of [["js", "text/javascript"], ["css", "text/css"]]) {
    const sha256 = createHash("sha256").update("").digest("hex"), file = `client.${sha256}.${extension}`;
    await writeFile(path.join(root, "web/dist/human-ui", file), ""); assets.push({ name: `client.${extension}`, path: file, sha256, content_type });
  }
  await writeFile(path.join(root, "web/dist/human-ui/manifest.json"), JSON.stringify({ assets }));
  for (const name of ["logo.svg", "logo-dark.svg"]) await writeFile(path.join(root, "crates/erp-server/brand", `erpforall-${name}`), "<svg/>");
  await writeFile(path.join(root, "REVISION"), "a".repeat(40));
  return { root, output: path.join(root, "evidence") };
}
async function failedRun(root: string, output: string, browser: string, timeout = 8000) {
  try {
    await run(process.execPath, [path.join(root, "web/scripts/browser-human-ui.mjs")], {
      env: { ...process.env, CLIENT_BROWSER_CDP: "", CLIENT_BROWSER_OUTPUT: output, CHROME_BIN: browser },
      timeout, maxBuffer: 1024 * 1024,
    });
    throw new Error("Expected browser harness refusal");
  } catch (error) {
    return error as Error & { code?: number; killed?: boolean; stderr?: string };
  }
}
test("missing Chrome names the executable and terminates without a PASS receipt", async () => {
  const { root, output } = await fixture(), browser = path.join(root, "missing-chrome");
  const error = await failedRun(root, output, browser);
  expect(error.killed).not.toBe(true);
  expect(error.code).toBe(1);
  expect(error.stderr).toContain(`Chrome executable is unavailable: ${browser}`);
  await expect(readFile(path.join(output, "result.json"), "utf8")).rejects.toMatchObject({ code: "ENOENT" });
}, 15000);
test("a browser that exits before CDP is reported without hanging in teardown", async () => {
  const { root, output } = await fixture(), browser = path.join(root, "exits-chrome");
  await writeFile(browser, "#!/bin/sh\nexit 23\n", { mode: 0o700 });
  const error = await failedRun(root, output, browser);
  expect(error.killed).not.toBe(true);
  expect(error.code).toBe(1);
  expect(error.stderr).toContain("Chrome exited before connection: 23");
}, 15000);
test("an existing evidence directory is refused without preserving a stale PASS as a new result", async () => {
  const { root, output } = await fixture();
  await mkdir(output); await writeFile(path.join(output, "result.json"), "stale PASS");
  const error = await failedRun(root, output, path.join(root, "missing-chrome"));
  expect(error.killed).not.toBe(true);
  expect(error.code).toBe(1);
  expect(error.stderr).toContain("Evidence directory already exists:");
  expect(await readFile(path.join(output, "result.json"), "utf8")).toBe("stale PASS");
}, 15000);

test("a browser with no CDP endpoint reaches the startup deadline and releases its profile", async () => {
  const { root, output } = await fixture(), browser = path.join(root, "silent-chrome");
  await writeFile(browser, '#!/bin/sh\nfor arg do case "$arg" in --user-data-dir=*) printf "%s" "${arg#*=}" > "' + path.join(root, "profile.txt") + '" ;; esac; done\nexec /bin/sleep 30\n', { mode: 0o700 });
  const error = await failedRun(root, output, browser, 25000);
  expect(error.killed).not.toBe(true);
  expect(error.code).toBe(1);
  expect(error.stderr).toContain("Chrome startup deadline exceeded");
  const profile = await readFile(path.join(root, "profile.txt"), "utf8");
  await expect(readFile(path.join(output, "result.json"), "utf8")).rejects.toMatchObject({ code: "ENOENT" });
  expect(JSON.parse(await readFile(path.join(output, "failure.json"), "utf8"))).toMatchObject({ status: "FAIL", message: "Error: Chrome startup deadline exceeded" });
  const { access } = await import("node:fs/promises");
  await expect(access(profile)).rejects.toMatchObject({ code: "ENOENT" });
}, 35000);

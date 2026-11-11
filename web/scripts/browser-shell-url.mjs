/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual native server and compiled client. Never intercept RPC or log fixture credentials.
import assert from "node:assert/strict";
import { readFile, writeFile, mkdir, stat, realpath } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { hostname, homedir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import path from "node:path";

const root = fileURLToPath(new URL("../../", import.meta.url));
const contractPath = process.argv[2];
assert.ok(contractPath && process.argv.length === 3, "one private fixture contract required");
const info = await stat(contractPath);
assert.ok(info.isFile() && info.uid === process.getuid() && (info.mode & 0o777) === 0o600 && info.size <= 65536);
const contract = JSON.parse(await readFile(contractPath, "utf8"));
assert.equal(contract.schema, 1); assert.equal(contract.fixtureKind, "shell-url");
const origin = new URL(contract.url);
assert.ok(origin.protocol === "http:" && origin.hostname === "127.0.0.1" && origin.pathname === "/");
const revision = (await readFile(path.join(root, "REVISION"), "utf8")).trim();
assert.match(revision, /^[0-9a-f]{40}$/);
assert.equal(contract.sourceRevision, revision); assert.equal(contract.assetsRevision, revision);
const manifestBytes = await readFile(path.join(path.dirname(contractPath), "manifest.json"));
assert.equal(createHash("sha256").update(manifestBytes).digest("hex"), contract.assetsManifestSha256);
const manifest = JSON.parse(manifestBytes);
const qaRoot = path.join(homedir(), ".local/share/erpforall-qa"), qa = await stat(qaRoot);
assert.ok(qa.isDirectory() && qa.uid === process.getuid() && (qa.mode & 0o777) === 0o700);
const playwrightPath = await realpath(path.join(qaRoot, "node_modules/playwright/index.mjs"));
assert.ok(playwrightPath.startsWith((await realpath(qaRoot)) + path.sep));
const { chromium } = await import(pathToFileURL(playwrightPath).href);
const output = process.env.CLIENT_BROWSER_OUTPUT;
assert.ok(output, "CLIENT_BROWSER_OUTPUT must name this run's evidence directory");
await mkdir(output, { recursive: true, mode: 0o700 });
const result = { status: "FAIL", host: hostname(), time_utc: new Date().toISOString(), node: process.version,
  source_revision: revision, assets_manifest_sha256: contract.assetsManifestSha256,
  driver_sha256: createHash("sha256").update(await readFile(fileURLToPath(import.meta.url))).digest("hex"),
  scope: "Real initialized native server: filtered-list reload and expired copied method identity", checks: [], page_errors: [] };
let browser;
try {
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_BIN || "/usr/bin/google-chrome" });
  result.browser = browser.version();
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  const page = await context.newPage(); page.setDefaultTimeout(15000);
  page.on("pageerror", error => result.page_errors.push(error.message));
  await page.goto(new URL("/erp/login", origin).href);
  await page.locator('input[name="login"]').fill(contract.login);
  await page.locator('input[name="password"]').fill(contract.password);
  await Promise.all([page.waitForURL(url => url.pathname === "/erp"), page.getByRole("button", { name: "Sign in", exact: true }).click()]);
  const rpc = async (model, method, args) => {
    const reply = await context.request.post(new URL(`/erp/json/2/${model}/${method}`, origin).href,
      { data: args, headers: { "Sec-Fetch-Site": "same-origin" } });
    assert.equal(reply.status(), 200, `fixture read ${model}/${method} refused`);
    return reply.json();
  };
  const menus = await rpc("ir.ui.menu", "erp_load_web_menus", {});
  let actionId;
  for (const id of new Set(menus.nodes.map(menu => menu.action_id).filter(id => id !== null))) {
    const action = await rpc("ir.actions.act_window", "erp_load_web_action", { ids: [id] });
    if (action.model === contract.noteModel) { actionId = id; break; }
  }
  assert.ok(actionId, "installed note action required");
  const destination = new URL(`/erp/action-${actionId}`, origin);
  destination.searchParams.set("debug", "1"); destination.searchParams.set("view_type", "list");
  destination.searchParams.set("search_facets", JSON.stringify([{ field: "name", values: [contract.noteName], operator: "ilike" }]));
  await page.goto(destination.href);
  await page.locator(".list-table tbody tr").filter({ hasText: contract.noteName }).waitFor();
  assert.equal(await page.locator(".list-table tbody tr").count(), 1);
  await page.reload();
  await page.locator(".list-table tbody tr").filter({ hasText: contract.noteName }).waitFor();
  assert.equal(await page.locator(".list-table tbody tr").count(), 1);
  assert.equal(new URL(page.url()).searchParams.get("debug"), "1");
  assert.ok(new URL(page.url()).searchParams.get("search_facets")?.includes(contract.noteName));
  result.checks.push("served filtered list survives reload with presentation debug");
  await page.screenshot({ path: path.join(output, "filtered-reload.png") });
  // An absent page payload must fail before any destination record read or mutation.
  const calls = [];
  page.on("request", request => {
    const url = new URL(request.url()); if (url.pathname.startsWith("/erp/json/2/")) calls.push(url.pathname);
  });
  const copied = new URL(`/erp/action-${actionId}`, origin);
  copied.searchParams.set("method_action", `${randomUUID()}:1`); copied.searchParams.set("view_type", "list");
  await page.goto(copied.href);
  await page.getByRole("link", { name: "Return to originating action", exact: true }).waitFor();
  assert.ok(await page.getByText("This related action has expired. Open its originating menu again.", { exact: true }).isVisible());
  assert.deepEqual(calls.filter(call => !call.endsWith("/erp_load_web_menus") && !call.endsWith("/erp_describe") && !call.endsWith("/erp_read_values")), []);
  result.checks.push("copied method identity expires without source method or destination record RPC");
  await page.screenshot({ path: path.join(output, "expired-method.png") });
  // Verify the served release assets correspond to this fixture's manifest.
  for (const asset of manifest.assets) {
    const reply = await context.request.get(new URL(`/erp/assets/${asset.path}`, origin).href);
    assert.equal(reply.status(), 200);
    assert.equal(createHash("sha256").update(await reply.body()).digest("hex"), asset.sha256);
  }
  assert.deepEqual(result.page_errors, []);
  result.status = "PASS";
} finally {
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n", { mode: 0o600 });
  await browser?.close();
}

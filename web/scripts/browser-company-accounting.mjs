/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual task-native server and installed company form; no intercepted RPCs or synthetic metadata.
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { hostname } from "node:os";
import path from "node:path";
import { chromium } from "playwright";
let input = "";
for await (const chunk of process.stdin) input += chunk;
const { origin, cookies, action, company } = JSON.parse(input);
const output = process.env.ACCOUNT_COMPANY_OUTPUT;
assert.ok(output, "ACCOUNT_COMPANY_OUTPUT is required");
await mkdir(output); // Refuse stale captures or an earlier successful receipt.
const executablePath = process.env.CHROME_BIN ?? (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const browser = await chromium.launch({ executablePath, headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
const stamp = { host: hostname(), utc: new Date().toISOString(), node: process.version, browser: await browser.version() };
let page;
try {
  const context = await browser.newContext();
  await context.addCookies(cookies.split("; ").map(cookie => {
    const i = cookie.indexOf("=");
    return { name: cookie.slice(0, i), value: cookie.slice(i + 1), url: origin };
  }));
  page = await context.newPage();
  const errors = [];
  page.on("pageerror", error => errors.push(String(error)));
  await page.goto(`${origin}/erp/action-${action}?id=${company}&view_type=form`, { waitUntil: "networkidle" });
  const prefix = page.locator('input[name="bank_account_code_prefix"]');
  await prefix.waitFor({ state: "visible" });
  assert.equal(await page.getByRole("button", { name: "New", exact: true }).count(), 0, "configuration opens an existing company only");
  await page.locator('[data-field="chart_template"]').waitFor({ state: "visible" });
  assert.equal(await page.locator('[data-field="chart_template"] input').count(), 0, "chart remains readonly");
  await prefix.fill("BRW");
  await prefix.press("Tab");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await page.waitForLoadState("networkidle");
  await page.reload({ waitUntil: "networkidle" });
  assert.equal(await prefix.inputValue(), "BRW", "save survives native server reload");
  await prefix.fill("DISCARD");
  await prefix.press("Tab");
  await page.getByRole("button", { name: "Discard", exact: true }).click();
  await page.waitForLoadState("networkidle");
  assert.equal(await prefix.inputValue(), "BRW", "discard restores saved state");
  await prefix.fill("bad/prefix");
  await prefix.press("Tab");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await page.getByText("bank_account_code_prefix must be empty or contain only ASCII letters, digits, dots and dashes, up to 64 characters.", { exact: true }).waitFor();
  assert.equal(await prefix.inputValue(), "bad/prefix", "failed save retains the draft");
  await page.locator("[data-error-close]").click();
  await page.getByRole("button", { name: "Discard", exact: true }).click();
  const transfer = page.locator('input[name="transfer_account_id"]');
  await transfer.fill("Browser transfer");
  await page.getByRole("option").filter({ hasText: "Browser transfer account" }).waitFor();
  await transfer.press("ArrowDown");
  await transfer.press("Enter");
  await transfer.press("Tab");
  assert.match(await transfer.inputValue(), /Browser transfer account/);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await page.waitForLoadState("networkidle");
  await page.reload({ waitUntil: "networkidle" });
  assert.match(await transfer.inputValue(), /Browser transfer account/, "keyboard selection persists");
  for (const width of [1440, 800, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await prefix.waitFor({ state: "visible" });
    await page.screenshot({ path: path.join(output, `company-${width}.png`), fullPage: true });
  }
  assert.deepEqual(errors, []);
  const result = { ...stamp, status: "PASS", server: "actual installed native account addon", save_reload: true,
    discard: true, failed_save_retains_draft: true, readonly_chart: true, keyboard_relation_saved: true,
    screenshots: [1440, 800, 390], performance: "DEFERRED_PERFORMANCE" };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify(result));
  await context.close();
} catch (error) {
  await writeFile(path.join(output, "failure.json"), JSON.stringify({ ...stamp, status: "FAIL", message: String(error) }, null, 2) + "\n");
  if (page) await page.screenshot({ path: path.join(output, "failure.png"), fullPage: true });
  throw error;
} finally { await browser.close(); }

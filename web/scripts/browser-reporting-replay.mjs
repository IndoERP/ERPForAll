/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Driven by the native human_reporting_export fixture. No response is intercepted.
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const { origin, cookies, action, menu, output } = JSON.parse(input);
assert.match(origin, /^http:\/\/127\.0\.0\.1:\d+$/);
await mkdir(output, { recursive: true });

const executablePath = process.env.CHROME_BIN ?? (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const browser = await chromium.launch({ executablePath, headless: true });
const context = await browser.newContext({ acceptDownloads: true });
await context.addCookies(cookies.split("; ").map(cookie => {
  const separator = cookie.indexOf("=");
  return { name: cookie.slice(0, separator), value: cookie.slice(separator + 1), url: origin };
}));
const page = await context.newPage();
page.setDefaultTimeout(30_000);
const clientErrors = [];
page.on("pageerror", error => clientErrors.push(error.message));
const row = name => page.locator("tr.report-line").filter({
  has: page.locator(".report-label").and(page.getByText(name, { exact: true })),
});
const labels = () => page.locator("tr.report-line").evaluateAll(rows => rows.map(line => {
  const more = line.querySelector(".report-more");
  return more ? more.textContent.replace(/^Show more \((\d+) remaining\)$/, "$1 more")
    : line.querySelector(".report-label")?.textContent;
}));
try {
  await page.goto(origin + "/erp/action-" + action + "?menu=" + menu);
  await page.locator(".report-table").waitFor();
  await page.getByRole("button", { name: "Unfold On Hand", exact: true }).click();
  await row("B10").waitFor();
  assert.equal(await row("B11").count(), 0);
  await page.getByRole("button", { name: "Show more (2 remaining)", exact: true }).click();
  await page.getByRole("button", { name: "Unfold B11", exact: true }).click();
  await row("Visible10").waitFor();
  assert.equal(await row("Visible11").count(), 0);
  await page.getByRole("button", { name: "Show more (13 remaining)", exact: true }).click();
  await row("Visible20").waitFor();
  const before = await labels();
  const position = before.indexOf("B11");
  assert.ok(position > 0);
  const children = Array.from({ length: 20 }, (_, i) => "Visible" + String(i + 1).padStart(2, "0"));
  assert.deepEqual(before.slice(position + 1, position + 22), [...children, "3 more"]);
  assert.ok(!before.includes("NoAccess") && !before.includes("Visible21"));
  assert.deepEqual((await row("Total On Hand").locator(".report-cell").allTextContents()).map(Number), [170, 34, 136]);
  const branchKeys = await Promise.all([row("On Hand").getAttribute("data-id"), row("B11").getAttribute("data-id")]);

  const [drill] = await Promise.all([
    page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/erp_report_drill")),
    row("Visible20").locator("[data-drill]").first().click(),
  ]);
  assert.equal(drill.status(), 200, await drill.text());
  await page.locator(".report-table").waitFor({ state: "detached" });
  await page.getByText("Visible20", { exact: true }).waitFor();
  await page.locator(".breadcrumb-origin").click();
  await row("Visible20").waitFor();
  assert.deepEqual(await labels(), before);
  await page.screenshot({ path: path.join(output, "restored-report.png"), fullPage: true });
  for (const format of ["pdf", "xlsx"]) {
    const [download, exportResponse] = await Promise.all([
      page.waitForEvent("download"),
      page.waitForResponse(response => new URL(response.url()).pathname === "/erp/reporting/export"),
      page.getByRole("button", { name: format.toUpperCase(), exact: true }).click(),
    ]);
    assert.equal(exportResponse.status(), 200);
    const request = exportResponse.request().postDataJSON();
    assert.deepEqual(Object.keys(request).sort(), ["format", "options", "report"]);
    assert.equal(request.format, format);
    assert.deepEqual(request.options.loaded_pages, Object.fromEntries(branchKeys.map(key => [key, 1])));
    assert.equal(download.suggestedFilename(), "Fixture Stock." + format);
    await download.saveAs(path.join(output, "replay." + format));
  }
  assert.deepEqual(clientErrors, []);
  await writeFile(path.join(output, "rows.json"), JSON.stringify(before) + "\n");
} catch (error) {
  console.error(error);
  await page.screenshot({ path: path.join(output, "failed-report.png"), fullPage: true });
  console.error((await page.locator("body").innerText()).slice(0, 2500));
  process.exitCode = 1;
} finally {
  await browser.close();
}

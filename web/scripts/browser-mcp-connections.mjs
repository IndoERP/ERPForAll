/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Disposable native tenant, enabled programmatic issuance, ordinary employee credentials on stdin.
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { chromium } from "playwright";
let input = "";
for await (const chunk of process.stdin) input += chunk;
const { origin, login, password } = JSON.parse(input);
input = "";
const browser = await chromium.launch({ headless: true,
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome") });
const context = await browser.newContext();
const page = await context.newPage();
page.setDefaultTimeout(20000);
let phase = "login";
try {
  // Clipboard is private to this browser fixture. Nothing reaches the host clipboard or logs.
  await page.addInitScript(() => {
    let last = "";
    Object.defineProperty(navigator, "clipboard", { value: { writeText: async v => { last = v; } } });
    window.checkCopied = value => value === last && last.length > 0;
    window.clearCopied = () => { last = ""; };
  });
  await page.goto(origin + "/erp/login");
  await page.locator('input[name="login"]').fill(login);
  await page.locator('input[name="password"]').fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator("[data-client-content]").waitFor();
  const account = page.getByRole("dialog", { name: "My Preferences" });
  async function preferences() {
    phase = "open account menu";
    await page.locator(".user-button").click();
    phase = "choose preferences";
    await page.getByRole("menuitem", { name: /my preferences/i }).click();
    phase = "choose account security";
    await account.getByRole("tab", { name: "Account Security" }).click();
    phase = "load connections";
    await account.getByRole("button", { name: "Manage MCP connections" }).click();
    await account.getByRole("button", { name: "New MCP connection" }).waitFor();
  }
  phase = "open preferences";
  await preferences();
  const name = "Browser acceptance " + randomUUID().slice(0, 8);
  await account.getByRole("button", { name: "New MCP connection" }).click();
  phase = "connection name";
  await account.getByLabel("Connection name").fill(name);
  phase = "read-only default";
  assert.equal(await account.getByRole("combobox", { name: /Access/ }).inputValue(), "read");
  phase = "open scope options";
  await account.getByText("Restrict models and methods", {exact:true}).click();
  phase = "choose scope";
  await account.getByLabel("Models", {exact:true}).fill("res.partner");
  await account.getByLabel("Methods", {exact:true}).fill("search_read");
  phase = "confirm identity";
  await account.getByLabel("Confirm your ERP password").fill(password);
  phase = "create connection";
  const response = page.waitForResponse(r => r.url().endsWith("/erp/session/api-key"));
  await account.getByRole("button", { name: "Create connection" }).click();
  assert.equal((await response).status(), 200);
  const once = account.getByLabel("One-time token");
  await once.waitFor();
  await account.getByRole("button", { name: "Copy token" }).click();
  assert.ok(await once.evaluate(node => window.checkCopied(node.value)));
  let bearer = await once.inputValue();
  const ping = async () => {
    const r = await fetch(origin + "/erp/mcp", { method: "POST", headers: {
      Authorization: "Bearer " + bearer, "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc:"2.0",id:1,method:"ping" }) });
    return r.status;
  };
  assert.equal(await ping(),200);
  await page.evaluate(() => window.clearCopied());
  await account.getByRole("button", { name: "Done" }).click();
  await account.locator(".wizard-footer").getByRole("button", { name: "Cancel" }).click();
  phase = "reopen";
  await preferences();
  await account.getByRole("button", { name: "Revoke " + name, exact:true }).waitFor();
  assert.equal(await once.count(),0);
  assert.ok(await account.evaluate((node, token) => !node.textContent.includes(token) && ![...node.querySelectorAll("input")].some(i => i.value === token), bearer));
  phase = "revoke";
  await account.getByRole("button", { name: "Revoke " + name, exact:true }).click();
  await account.getByRole("button", { name:"Confirm revocation" }).click();
  await account.getByText("Revoked " + name + ".", {exact:true}).waitFor();
  assert.equal(await ping(),401); bearer = "";
  process.stdout.write("MCP browser lifecycle passed: scoped issuance, one-time copy, reopened inventory and immediate revocation.\n");
} catch {
  // Some Playwright errors include the full inputs or request headers.
  throw new Error("MCP browser lifecycle failed at " + phase);
} finally {
  await browser.close();
}

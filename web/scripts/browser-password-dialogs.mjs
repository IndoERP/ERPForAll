/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled client and installed base on the native HTTP fixture. No intercepted requests.
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const { origin, cookies, action, target, actor, oldTarget, newTarget, newActor, officeBlue } = JSON.parse(input);
const executablePath = process.env.CHROME_BIN ?? (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const output = process.env.ERP_PASSWORD_BROWSER_OUTPUT;
if (output) await mkdir(output, { recursive: true });
async function captureEmpty(dialog, name) {
  if (!output) return;
  assert.ok(await dialog.locator("input").evaluateAll(fields => fields.every(field => !field.value)));
  await dialog.screenshot({ path: path.join(output, officeBlue ? `office-${name}` : name) });
}
const browser = await chromium.launch({ executablePath, headless: true });
let step = "start the fixture browser";
const operations = new Set(["run_check", "change_password_button", "change_password", "preference_change_password", "action_change_password_wizard"]);
const statuses = [];
try {
  const context = await browser.newContext();
  await context.addCookies(cookies.split("; ").map(cookie => {
    const cut = cookie.indexOf("=");
    return { name: cookie.slice(0, cut), value: cookie.slice(cut + 1), url: origin };
  }));
  const page = await context.newPage();
  page.setDefaultTimeout(15_000);
  page.on("response", response => {
    const method = new URL(response.url()).pathname.split("/").pop();
    if (operations.has(method) || method === "action_get" || response.status() >= 400 && response.url().startsWith(origin + "/erp/json/2/")) statuses.push(`${method} ${response.status()}`);
  });
  const route = `${origin}/erp/action-${action}?id=${target}&view_type=form`;
  await page.goto(route);
  await page.getByRole("button", { name: "Change password", exact: true }).click();
  assert.equal(await page.locator("html").getAttribute("data-office-theme"), officeBlue ? "office-blue" : null);
  const identity = page.getByRole("dialog", { name: "Confirm your identity" });
  const password = identity.locator("input[name=password]");
  const confirm = identity.getByRole("button", { name: "Confirm", exact: true });
  const submit = async (path, operation) => {
    const [reply] = await Promise.all([page.waitForResponse(r => r.url().endsWith(path)), operation()]);
    return reply.status();
  };
  const endpoint = "/res.users.identitycheck/run_check";
  const checks = [];
  page.on("request", request => {
    if (!request.url().endsWith(endpoint)) return;
    const supplied = request.postDataJSON()?.password;
    checks.push(supplied === oldTarget ? "target" : supplied === actor ? "actor" : "unexpected");
  });
  step = "identify the actor and reject the target's current password";
  await identity.getByText(/Enter your current password to continue as /).waitFor();
  assert.equal(await password.getAttribute("autocomplete"), "current-password");
  assert.ok((await identity.boundingBox()).width <= 480);
  await captureEmpty(identity, "password-identity.png");
  const reveal = identity.getByRole("button", { name: /Show .*password/i });
  assert.equal(await reveal.count(), 1);
  await reveal.click();
  assert.equal(await password.getAttribute("type"), "text");
  await reveal.click();
  await password.fill(oldTarget);
  assert.equal(await submit(endpoint, () => confirm.click()), 403);
  await identity.getByRole("alert").getByText(/current password/).waitFor();
  assert.ok(await password.evaluate(field => field === document.activeElement && field.value === ""));
  step = "confirm the actor by clicking with a password-manager value";
  // Exercise the filled DOM state with no input/change event.
  await password.evaluate((field, value) => { field.value = value; }, actor);
  assert.equal(await submit(endpoint, () => confirm.click()), 200);
  assert.deepEqual(checks, ["target", "actor"]);
  step = "change another account's password in the parent dialog";
  const reset = page.getByRole("dialog", { name: "Reset passwords" });
  await reset.getByText("Reset Target (password-target)", { exact: true }).waitFor();
  const replacement = reset.locator("input[type=password]");
  assert.equal(await replacement.count(), 1);
  await captureEmpty(reset, "password-reset.png");
  await replacement.fill(newTarget);
  assert.equal(await submit("/change.password.wizard/change_password_button", () => reset.getByRole("button", { name: "Reset passwords" }).click()), 200);
  await reset.waitFor({ state: "hidden" });
  step = "open the actor's password change in a fresh session";
  await context.clearCookies();
  await page.goto(`${origin}/erp/login`);
  await page.locator("#login-name").fill("fixture-admin");
  await page.locator("#login-password").fill(actor);
  await Promise.all([
    page.waitForURL(url => url.pathname !== "/erp/login"),
    page.locator("[data-login-submit]").click(),
  ]);
  await page.goto(route);
  await page.getByRole("button", {name: "Change password", exact: true}).waitFor();
  step = "toggle the user profile menu";
  await page.locator(".user-button").click();
  step = "select My preferences";
  await page.locator(".user-dropdown-preferences").click();
  step = "start Change password from My preferences";
  await page.getByRole("dialog", { name: "My Preferences" }).getByRole("button", { name: "Change password" }).click();
  step = "confirm the signed-in actor in My preferences with Enter";
  await password.fill(actor);
  assert.equal(await submit(endpoint, () => password.press("Enter")), 200);
  assert.deepEqual(checks, ["target", "actor", "actor"]);
  const own = page.getByRole("dialog", { name: "Change password" });
  const first = own.locator("input[name=new_password]"), second = own.locator("input[name=confirm_password]");
  await first.waitFor();
  await captureEmpty(own, "password-own.png");
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok((await own.boundingBox()).width <= 390);
  assert.ok(await own.evaluate(dialog => dialog.scrollWidth <= dialog.clientWidth));
  assert.equal(await first.getAttribute("autocomplete"), "new-password");
  assert.equal(await second.getAttribute("autocomplete"), "new-password");
  step = "submit the matching new password and confirmation via Enter";
  await first.fill(newActor);
  await second.evaluate((field, value) => { field.value = value; }, newActor);
  assert.equal(await submit("/change.password.own/change_password", () => second.press("Enter")), 200);
  await own.waitFor({ state: "hidden" });
} catch {
  // Browser action traces and raw RPC payloads can contain credentials.
  throw new Error(`Password browser check failed during ${step}. Operations: ${statuses.join(", ")}`);
} finally {
  await browser.close();
}

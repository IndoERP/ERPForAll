/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The native fixture supplies disposable accounts. Never print credentials or response bodies.
import assert from "node:assert/strict";
import { join } from "node:path";
import { chromium } from "playwright";
let input = "";
for await (const part of process.stdin) input += part;
const { origin, requester, reviewer, password, finalReviewer, finalPassword, configurationAction, configurationMenu, legacyRequestAction, legacyConfigurationAction } = JSON.parse(input);
const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome"),
  headless: true,
});
async function signIn(login, secret = password) {
  const context = await browser.newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);
  await page.goto(origin + "/erp/login");
  await page.locator('input[name="login"]').fill(login);
  try {
    await page.locator('input[name="password"]').fill(secret);
  } catch {
    throw new Error("The sign-in password input is unavailable.");
  }
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator(".user-button").waitFor();
  return page;
}
try {
  const [sender, approver, compliance] = await Promise.all([signIn(requester), signIn(reviewer), signIn(finalReviewer, finalPassword)]);
  const policy = "Contact change approval";
  const screenshots = process.env.ERP_APPROVAL_BROWSER_OUTPUT;
  const workflows = `${origin}/erp/action-${configurationAction}?menu=${configurationMenu}`;
  const button = (page, name) => page.getByRole("button", { name, exact: true });
  const row = (page, name) => page.getByRole("row").filter({ has: page.getByText(name, { exact: true }) });
  const screenshot = async (page, name) => {
    if (screenshots) await page.screenshot({ path: join(screenshots, `approval-${name}.png`), animations: "disabled" });
  };
  const fit = async page => {
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "the view must fit its viewport");
  };
  async function pick(page, field, query, label = query) {
    await page.locator(`input[name="${field}"]`).fill(query);
    await page.getByRole("listbox").getByRole("option", { name: label, exact: true }).click();
  }
  async function saveWorkflow(page) {
    const saved = page.waitForResponse(reply => reply.url().endsWith("/approval.workflow/web_save"), { timeout: 10_000 });
    await button(page, "Save").click();
    const reply = await saved;
    if (!reply.ok()) {
      const error = await reply.json();
      throw new Error(`Saving workflow returned ${reply.status()}: ${error.message ?? error.name}`);
    }
    await button(page, "Edit").waitFor();
  }
  await approver.goto(workflows);
  await button(approver, "New").click();
  const workflowName = approver.locator('input[name="name"]');
  await workflowName.fill(policy);
  // Saving an incomplete draft and reopening it must not activate any policy.
  await saveWorkflow(approver);
  await approver.reload();
  await button(approver, "Edit").click();
  await pick(approver, "model_id", "res.users", "User");
  const method = approver.locator('input[name="method"]');
  await method.fill("web_save");
  await approver.getByRole("option", { name: "web_save", exact: true }).waitFor();
  await method.press("Enter");
  assert.equal(await method.inputValue(), "web_save");
  async function step(order, name, user) {
    await button(approver, "Add a line").click();
    const dialog = approver.getByRole("dialog");
    await dialog.locator('input[name="name"]').fill(name);
    await dialog.locator('input[name="sequence"]').fill(String(order));
    await pick(approver, "user_ids", user);
    await button(dialog, "Save & Close").click();
    await dialog.waitFor({ state: "hidden" });
  }
  await step(10, "Team manager", reviewer);
  await step(20, "Compliance", finalReviewer);
  await fit(approver);
  await screenshot(approver, "workflow-editor");
  await approver.setViewportSize({ width: 390, height: 844 });
  await fit(approver);
  await screenshot(approver, "workflow-mobile");
  await approver.setViewportSize({ width: 1280, height: 800 });
  await saveWorkflow(approver);
  await button(approver, "Publish").click();
  await button(approver, "Create revision").waitFor();
  await screenshot(approver, "workflow");
  await approver.goto(`${origin}/erp/action-${legacyConfigurationAction}?menu=${configurationMenu}`);
  await row(approver, policy).waitFor();
  assert.equal(new URL(approver.url()).pathname, new URL(workflows).pathname);
  await screenshot(approver, "workflows");
  await sender.locator(".activity-menu-trigger").click();
  await sender.getByLabel("Notifications, 1 unread", { exact: true }).click();
  await sender.getByRole("button", { name: "Mark Delivery scheduled as read" }).click();
  await sender.locator(".activity-menu-trigger").click();
  await sender.getByLabel("Notifications, 0 unread", { exact: true }).waitFor();
  await sender.locator(".activity-menu-trigger").click();
  await sender.locator(".user-button").click();
  await sender.getByRole("menuitem", { name: "My preferences" }).click();
  const form = sender.getByRole("dialog", { name: "My Preferences" });
  await form.locator('input[name="phone"]').fill("12345678");
  await form.getByRole("button", { name: "Update Preferences" }).click();
  const pending = sender.getByRole("dialog", { name: /^Approval request \d+$/ });
  await pending.waitFor();
  const id = (await pending.getByRole("heading").innerText()).match(/\d+$/)?.[0];
  assert.ok(id);

  const clock = approver.locator(".activity-menu-trigger");
  await clock.click();
  const approvals = approver.getByLabel("Approvals, 1 to review", { exact: true });
  await approvals.waitFor();
  if (screenshots) await approver.screenshot({ path: join(screenshots, "approval-clock.png") });
  await approvals.click();
  const requestName = `Request #${id}`;
  await row(approver, requestName).waitFor();
  await screenshot(approver, "queue");
  await row(approver, requestName).click();
  const approve = button(approver, "Approve");
  await approve.waitFor();
  await approver.reload();
  await approve.waitFor();
  await screenshot(approver, "detail");
  await approver.setViewportSize({ width: 390, height: 844 });
  await fit(approver);
  await screenshot(approver, "mobile");
  await approve.click();
  await approve.waitFor({ state: "hidden" });
  await pending.getByRole("button", { name: "Check approval" }).click();
  await pending.getByText("Waiting for a reviewer. Stage 2 of 2 · Compliance").waitFor();

  await compliance.locator(".activity-menu-trigger").click();
  await compliance.getByLabel("Approvals, 1 to review", { exact: true }).click();
  await row(compliance, requestName).click();
  await button(compliance, "Approve").waitFor();
  await row(compliance, "Team manager").getByText(reviewer, { exact: true }).waitFor();
  await screenshot(compliance, "progress");
  await button(compliance, "Approve").click();
  await button(compliance, "Approve").waitFor({ state: "hidden" });

  const written = sender.waitForResponse(r => r.url().endsWith("/res.users/web_save") && r.status() === 200);
  await pending.getByRole("button", { name: "Check approval" }).click();
  await written;
  // The original request bookmark still opens the completed native form for its requester.
  await sender.goto(`${origin}/erp/action-${legacyRequestAction}?id=${id}&view_type=form`);
  await row(sender, "Compliance").getByText(finalReviewer, { exact: true }).waitFor();
  assert.notEqual(new URL(sender.url()).pathname, `/erp/action-${legacyRequestAction}`);
  assert.equal(await button(sender, "Approve").count(), 0);
  await fit(sender);

  await approver.setViewportSize({ width: 1280, height: 800 });
  await approver.goto(workflows);
  await row(approver, policy).click();
  await button(approver, "Create revision").click();
  await button(approver, "Publish revision").waitFor();
  await row(approver, "Compliance").waitFor();
  await button(approver, "Publish revision").click();
  await button(approver.getByRole("dialog"), "Publish revision").click();
  await button(approver, "Create revision").waitFor();
  await approver.goto(workflows);
  await row(approver, policy).getByText("Archived", { exact: true }).waitFor();
  await row(approver, policy).getByText("Active", { exact: true }).waitFor();
  console.log("Workflow configuration and two-stage approval completed in three Chrome sessions.");
} catch (error) {
  const destination = process.env.ERP_APPROVAL_BROWSER_OUTPUT;
  if (destination) for (const [index, context] of browser.contexts().entries()) {
    const page = context.pages()[0];
    if (page && /^\/erp\/action-\d+$/.test(new URL(page.url()).pathname)) {
      await page.screenshot({ path: join(destination, `approval-test-failure-${index}.png`) });
    }
  }
  throw error;
} finally {
  await browser.close();
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The native fixture supplies two disposable accounts. Never print credentials or response bodies.
import assert from "node:assert/strict";
import { chromium } from "playwright";
let input = "";
for await (const part of process.stdin) input += part;
const { origin, requester, reviewer, password } = JSON.parse(input);
const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome"),
  headless: true,
});
async function signIn(login) {
  const context = await browser.newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);
  await page.goto(origin + "/erp/login");
  await page.locator('input[name="login"]').fill(login);
  try {
    await page.locator('input[name="password"]').fill(password);
  } catch {
    throw new Error("The sign-in password input is unavailable.");
  }
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator(".user-button").waitFor();
  return page;
}
try {
  const [sender, approver] = await Promise.all([signIn(requester), signIn(reviewer)]);
  await sender.locator(".user-button").click();
  await sender.getByRole("menuitem", { name: "My preferences" }).click();
  const form = sender.getByRole("dialog", { name: "My Preferences" });
  await form.locator('input[name="phone"]').fill("12345678");
  await form.getByRole("button", { name: "Update Preferences" }).click();
  const pending = sender.getByRole("dialog", { name: /^Approval request \d+$/ });
  await pending.waitFor();
  const id = (await pending.getByRole("heading").innerText()).match(/\d+$/)?.[0];
  assert.ok(id);
  await approver.locator(".user-button").click();
  await approver.getByRole("menuitem", { name: "Review approval" }).click();
  const review = approver.getByRole("dialog", { name: "Review approval" });
  await review.getByLabel("Request number").fill(id);
  await review.getByRole("button", { name: "Open request" }).click();
  await review.getByText(/res\.users\.web_save/).waitFor();
  await review.getByRole("button", { name: "Approve", exact: true }).click();
  await review.getByText("Status: approved.").waitFor();
  const written = sender.waitForResponse(r => r.url().endsWith("/res.users/web_save") && r.status() === 200);
  await pending.getByRole("button", { name: "Check approval" }).click();
  await written;
  await review.getByRole("button", { name: "Open request" }).click();
  await review.getByText("Status: executed.").waitFor();
  await review.getByText(/^Execution receipt:/).waitFor();
  console.log("Two-user approval completed in Chrome.");
} finally {
  await browser.close();
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { chromium } from "playwright";
import assert from "node:assert/strict";

let input = "";
for await (const part of process.stdin) input += part;
const { origin, login, password } = JSON.parse(input);
const browser = await chromium.launch({
  headless: true,
  executablePath:
    process.env.CHROME_BIN ??
    (process.platform === "darwin"
      ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      : "/usr/bin/google-chrome"),
});
try {
  const context = await browser.newContext();
  context.setDefaultTimeout(25000);
  const first = await context.newPage();
  await first.goto(origin + "/erp/login");
  await first.locator('input[name="login"]').fill(login);
  try {
    await first.locator('input[name="password"]').fill(password);
  } catch {
    throw new Error("Sign-in input unavailable");
  }
  await first.getByRole("button", { name: "Sign in", exact: true }).click();
  await first.locator(".user-button").waitFor();
  const second = await context.newPage();
  await second.goto(origin + "/erp");
  const activityMenu = first.locator(".activity-menu-trigger");
  await activityMenu.waitFor();
  const before = Number((await first.locator(".activity-menu-counter").allTextContents())[0] ?? 0);
  await second.evaluate(async () => {
    const uid = JSON.parse(document.querySelector("#erp-bootstrap").textContent).identity.uid;
    const response = await fetch("/erp/json/2/mail.activity/create", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        vals_list: [{ summary: "Review discussion", date_deadline: "2000-01-01", user_id: uid }],
      }),
    });
    if (!response.ok) throw new Error("Could not schedule the activity.");
  });
  await activityMenu.click();
  await first.waitForFunction(
    (expected) => Number(document.querySelector(".activity-menu-counter")?.textContent) === expected,
    before + 1,
  );
  await activityMenu.click();
  const discuss = (page) => page.getByRole("region", { name: "Discuss", exact: true });
  for (const tab of [first, second]) await tab.getByRole("button", { name: "Discuss", exact: true }).click();
  const name = "Live meeting";
  await discuss(first).getByRole("textbox", { name: "Conversation name" }).fill(name);
  await discuss(first).getByRole("button", { name: "Create conversation" }).click();
  const choice = (tab) => discuss(tab).getByRole("button", { name: /^Live meeting ·/ });
  for (const tab of [first, second]) await choice(tab).click();
  const text = "Shipping is ready for review";
  const composer = discuss(first).locator(".erp-mail-composer textarea");
  await composer.fill(text);
  await composer.press("Control+Enter");
  await discuss(second).locator(".erp-mail-message").filter({ hasText: text }).waitFor();
  await discuss(second).getByRole("button", { name: "Open chat window" }).click();
  const chat = second.getByRole("region", { name: "Chat with Live meeting" });
  await chat.locator(".erp-mail-message").filter({ hasText: text }).waitFor();
  await discuss(first).getByRole("button", { name: "Save latest", exact: true }).click();
  await discuss(second)
    .getByRole("button", { name: /^Live meeting · .*1 saved/ })
    .waitFor();
  await second.reload();
  await second.getByRole("button", { name: "Discuss", exact: true }).click();
  await choice(second).click();
  await discuss(second).locator(".erp-mail-message").filter({ hasText: text }).waitFor();
  assert.equal(await discuss(second).locator(".erp-mail-message").filter({ hasText: text }).count(), 1);
  await discuss(first).getByRole("button", { name: "Leave conversation" }).click();
  await choice(second).waitFor({ state: "detached" });
  assert.equal(await discuss(second).getByText(text).count(), 0);
  await discuss(first).getByRole("textbox", { name: "Conversation name" }).fill("Session-only discussion");
  await discuss(first).getByRole("button", { name: "Create conversation" }).click();
  const sessionChannel = discuss(second).getByRole("button", { name: /^Session-only discussion ·/ });
  await sessionChannel.waitFor();
  await discuss(first).getByRole("button", { name: "Close", exact: true }).click();
  await first.locator(".user-button").click();
  await first.locator('form[action="/erp/session/logout"] button').click();
  await sessionChannel.waitFor({ state: "detached", timeout: 45000 });
  assert.equal(await second.getByText("Session-only discussion", { exact: true }).count(), 0);
} finally {
  await browser.close();
}

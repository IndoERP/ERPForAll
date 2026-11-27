/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { chromium } from "playwright";
import assert from "node:assert/strict";
import { join } from "node:path";

let input = "";
for await (const part of process.stdin) input += part;
const { origin, login, password, peers } = JSON.parse(input);
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
  const button = (root, name) => root.getByRole("button", { name, exact: true });
  const sidebar = tab => discuss(tab).locator(".discuss-sidebar");
  const channel = (tab, name) => sidebar(tab).locator(".discuss-row").filter({ has: tab.getByText(name, { exact: true }) });
  const name = "Live meeting";
  const text = "Shipping is ready for review";
  const message = tab => discuss(tab).locator(".erp-mail-message").filter({ hasText: text });
  async function choose(tab, names) {
    await button(sidebar(tab), "New message").click();
    const picker = discuss(tab).getByRole("form", { name: "New message" });
    const search = picker.getByRole("searchbox", { name: "Search people" });
    assert.equal(await search.evaluate(node => node === document.activeElement), true);
    for (const person of names) {
      await search.fill(person);
      await button(picker, person).click();
    }
    return picker;
  }
  // The workspace and quick switcher use the same authenticated channel and composer.
  for (const tab of [first, second]) await button(tab.locator(".discuss-control"), "Discuss").click();
  await button(sidebar(first), "Open workspace").focus();
  await first.keyboard.press("Enter");
  await first.locator("[data-client-content] .discuss-workspace").waitFor();
  assert.match(new URL(first.url()).pathname, /\/erp\/action-\d+$/);
  const route = first.url();
  const group = await choose(first, peers);
  await group.getByRole("textbox", { name: "Group name" }).fill(name);
  await button(group, "Create group").click();
  await discuss(first).getByRole("heading", { name, exact: true }).waitFor();
  await channel(second, name).click();
  const composer = discuss(first).locator(".erp-mail-composer textarea");
  await composer.fill(text);
  await first.locator("button.brand").click();
  await button(first.locator(".discuss-control"), "Discuss").click();
  assert.equal(await composer.inputValue(), text);
  await button(sidebar(first), "Open workspace").press("Enter");
  await first.locator("[data-client-content] .discuss-workspace").waitFor();
  await composer.press("Enter");
  await message(second).waitFor();
  if (process.env.DISCUSS_OUTPUT) await first.screenshot({ path: join(process.env.DISCUSS_OUTPUT, "discuss-desktop.png") });
  await button(discuss(second), "Open chat window").click();
  const chat = second.getByRole("region", { name: "Chat with Live meeting" });
  await chat.locator(".erp-mail-message").filter({ hasText: text }).waitFor();
  await message(first).hover();
  await button(message(first), "Save message").click();
  await button(sidebar(second), "Saved").click();
  await channel(second, name).getByText(text, { exact: true }).waitFor();
  await channel(second, name).click();
  await message(second).waitFor();
  await button(chat, "Close chat").click();
  // Keyboard navigation, reload and phone-width detail/back navigation use the same action.
  await first.reload();
  await first.locator("[data-client-content] .discuss-workspace").waitFor();
  assert.equal(first.url(), route);
  await first.setViewportSize({ width: 390, height: 844 });
  await channel(first, name).focus();
  await first.keyboard.press("Enter");
  await message(first).waitFor();
  if (process.env.DISCUSS_OUTPUT) await first.screenshot({ path: join(process.env.DISCUSS_OUTPUT, "discuss-mobile.png") });
  assert.equal(await first.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);
  await button(discuss(first), "Back to conversations").click();
  await sidebar(first).getByRole("searchbox", { name: "Search conversations" }).waitFor();
  const dm = await choose(first, [peers[0]]);
  await button(dm, "Start conversation").click();
  await discuss(first).getByRole("heading", { name: peers[0], exact: true }).waitFor();
  await button(discuss(first), "Back to conversations").click();
  await channel(first, name).click();
  await button(sidebar(second), "Conversations").click();
  await button(discuss(first), "Leave conversation").click();
  await channel(second, name).waitFor({ state: "detached" });
  assert.equal(await message(second).count(), 0);
  await first.setViewportSize({ width: 1280, height: 800 });
  await channel(second, peers[0]).waitFor();
  await button(sidebar(second), "New message").click();
  await second.keyboard.press("Escape");
  assert.equal(await button(sidebar(second), "New message").evaluate(node => node === document.activeElement), true);
  await first.locator(".user-button").click();
  await first.locator('form[action="/erp/session/logout"] button').click();
  await channel(second, peers[0]).waitFor({ state: "detached", timeout: 45000 });
} finally {
  await browser.close();
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Exercises ordinary installed form and content routes with two independent cookie jars.
import assert from "node:assert/strict";
import { chromium } from "playwright";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const { origin, action, company, officer, officerPassword, reader, readerPassword } = JSON.parse(input);
const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome"),
  headless: true,
});
async function signIn(login, password) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(origin + "/erp/login");
  await page.locator('input[name="login"]').fill(login);
  await page.locator('input[name="password"]').fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.locator(".user-button").waitFor();
  return page;
}
try {
  const output = await browser.newPage();
  await output.setContent('<p>Employment permit</p><p style="break-before:page">Second page</p>');
  const pdf = await output.pdf();
  await output.close();
  const [authorized, limited] = await Promise.all([signIn(officer, officerPassword), signIn(reader, readerPassword)]);
  const [id] = await authorized.evaluate(async ({ file, company }) => {
    const reply = await fetch("/erp/json/2/hr.employee/create", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ vals_list: [{ name: "Preview browser employee", company_id: company,
        work_permit_name: "permit.pdf", work_permit_file: file }] }),
    });
    if (!reply.ok) throw Error(`Employee fixture creation returned ${reply.status}`);
    return reply.json();
  }, { file: pdf.toString("base64"), company });
  assert.ok(Number.isSafeInteger(id) && id > 0);
  const path = `/erp/content/hr.employee/${id}/work_permit_file`;
  const denied = await limited.request.get(origin + path);
  assert.equal(denied.status(), 404);
  assert.ok(!(await denied.body()).includes(Buffer.from("%PDF-")));
  await authorized.goto(`${origin}/erp/action-${action}?id=${id}&view_type=form`);
  const personal = authorized.getByRole("tab", { name: "Personal", exact: true });
  const clientError = authorized.locator("#client-error-message");
  await personal.or(clientError).first().waitFor();
  if (await clientError.isVisible()) throw new Error(`Could not open employee form: ${await clientError.innerText()}`);
  await personal.click();
  const button = authorized.getByRole("button", { name: "Preview permit.pdf" });
  const fetched = authorized.waitForResponse(reply => new URL(reply.url()).pathname === path);
  await button.click();
  assert.equal((await fetched).status(), 200);
  await authorized.locator('.document-viewer-page[data-page="1"] canvas').waitFor();
  await authorized.getByRole("button", { name: "Next page" }).click();
  await authorized.locator('.document-viewer-page[data-page="2"] canvas').waitFor();
  assert.equal(await authorized.getByRole("textbox", { name: "Page", exact: true }).inputValue(), "2");
  assert.equal(await authorized.getByRole("button", { name: "Next page" }).isDisabled(), true);
  const zoom = authorized.locator(".document-viewer-zoom");
  const beforeZoom = Number.parseInt(await zoom.innerText(), 10);
  await authorized.getByRole("button", { name: "Zoom in" }).click();
  assert.ok(Number.parseInt(await zoom.innerText(), 10) > beforeZoom);
  await authorized.keyboard.press("Escape");
  await authorized.locator(".document-viewer").waitFor({ state: "detached" });
  assert.ok(await button.evaluate(node => node === document.activeElement));
  console.log("Installed authorized binary field rendered both PDF pages; field denial, zoom, Escape and focus passed.");
} finally {
  await browser.close();
}

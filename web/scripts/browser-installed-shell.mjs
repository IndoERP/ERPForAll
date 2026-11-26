/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Run through crates/erp-cli/tests/installed_shell_browser.rs against its disposable database.
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { chromium } from "playwright";
import axe from "axe-core";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const fixture = JSON.parse(input);
const output = process.env.SHELL_ACCEPTANCE_OUTPUT;
await mkdir(output, {recursive:true});
const browser = await chromium.launch({
  headless: true,
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome"),
});
const evidence = { browser: browser.version(), profile: fixture.profile, records: fixture.list_records, form_lines: fixture.lines, scenarios: [], audits: [], timings: [], status: "failed" };
let phase = "start";
const serverFailures = [];
const url = (action, id) => `${fixture.origin}/erp/action-${action}${id ? `?id=${id}&view_type=form` : ""}`;
async function signIn(login, password, width) {
  const context = await browser.newContext({viewport:{width,height:900}});
  // Install the audit observer via the browser tooling. The served application keeps its own CSP.
  await context.addInitScript({content:axe.source});
  const page = await context.newPage();
  page.on("response", response => {
    const target = new URL(response.url());
    if (target.origin === fixture.origin && response.status() >= 400 && serverFailures.length < 50) {
      serverFailures.push({ phase, path: target.pathname, status: response.status() });
    }
  });
  page.setDefaultTimeout(30_000);
  await page.goto(fixture.origin + "/erp/login");
  await page.locator("input[name=login]").fill(login);
  await page.locator("input[name=password]").fill(password);
  await page.locator("input[name=password]").press("Enter");
  await page.locator(".user-button").waitFor();
  return page;
}
async function audit(page, label, scope) {
  const violations = await page.evaluate(async scope => {
    const result = await window.axe.run(scope, {runOnly:{type:"tag",values:["wcag2a","wcag2aa","wcag21aa"]}});
    return result.violations.map(row => ({id:row.id,impact:row.impact,count:row.nodes.length,selectors:row.nodes.slice(0,3).map(node=>node.target)}));
  },scope);
  evidence.audits.push({label,violations});
}
async function measure(page, label, action) {
  const calls = [];
  const finished = response => {
    const target = new URL(response.url());
    if (target.origin === fixture.origin) calls.push({path:target.pathname,status:response.status(),resource:response.request().resourceType()});
  };
  page.on("response",finished);
  const start = performance.now();
  try {
    await action();
    await page.evaluate(() => new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
    const paint = performance.now() - start;
    await page.waitForLoadState("networkidle");
    evidence.timings.push({ label, ready_ms:Math.round(paint), requests:calls });
    assert.ok(calls.every(call=>call.status<400), label);
  } finally { page.off("response",finished); }
}
async function signOut(page, expected) {
  await page.locator(".user-button").click();
  const reply = page.waitForResponse(response => new URL(response.url()).pathname === "/erp/session/logout");
  await page.getByRole("menuitem",{name:"Sign out"}).press("Enter");
  const response = await reply;
  assert.equal(response.status(),expected);
  assert.ok((await response.headerValue("clear-site-data"))?.includes("storage"));
  await page.waitForLoadState("domcontentloaded");
  assert.ok(!(await page.context().cookies(fixture.origin)).some(row=>row.name==="session_id"));
  await page.goto(fixture.origin + "/erp");
  await page.locator("input[name=password]").waitFor();
}
async function checkRestrictedCards() {
  phase = "restricted-cards";
  const [admin,reader] = await Promise.all([
    signIn("fixture-admin","fixture-administrator-password",1440),
    signIn("shell-reader","fixture-shell-reader",390),
  ]);
  const page = await reader.goto(url(fixture.cards));
  assert.ok((await page.headerValue("content-security-policy")).includes("script-src"));

  const admittedCard = reader.locator(".kanban-card strong.shell-card-name");
  await admittedCard.waitFor();
  assert.equal(await admittedCard.count(), 1);
  assert.equal(await admittedCard.textContent(), "WASI port & quay");
  const restrictedDescription = page => page.request.post(fixture.origin + "/erp/json/2/res.partner/get_views", {
    headers: {"Sec-Fetch-Site": "same-origin"}, data: {views:[[fixture.refused_card,"kanban"]], wire_major:1, options:{}},
  });
  const allowed = await restrictedDescription(admin);
  assert.equal(allowed.status(), 200);
  const refused = await restrictedDescription(reader);
  assert.ok(refused.status() >= 400);
  assert.match(await refused.text(), /Restricted card in view [0-9]+ at [0-9]+:[0-9]+/);
  evidence.scenarios.push("Served WASI card rendered under CSP for one permitted contact. The protected-field card accepted the administrator and refused the reader with its source position");
}
async function checkShell() {
  phase = "admin-login";
  const admin = await signIn("fixture-admin","fixture-administrator-password",1440);
  evidence.scenarios.push("Administrator signed in with keyboard");
  phase = "long-list";
  await measure(admin,phase,async()=>{
    await admin.goto(url(fixture.contacts)); await admin.locator(".list-table tbody [data-record-open]").first().waitFor();
  });
  const size = await admin.locator(".list-table tbody tr").count();
  assert.ok(size > 0 && size < fixture.list_records);
  const firstRow = await admin.locator("[data-record-open]").first().getAttribute("data-record-open");
  await measure(admin,"long-list-next-page",async()=>{
    await admin.getByRole("button",{name:"Next page",exact:true}).press("Enter");
    await admin.waitForFunction(id=>document.querySelector("[data-record-open]")?.getAttribute("data-record-open")!==id,firstRow);
  });
  assert.ok(evidence.timings.at(-1).requests.some(r=>r.path.endsWith("/web_search_read")));
  evidence.scenarios.push(`Paged ${fixture.list_records} seeded contacts in ${size}-row pages`);
  await audit(admin,"desktop contact list",".list-table");
  phase = "large-form";
  await measure(admin,phase,async()=>{
    await admin.goto(url(fixture.sales,fixture.order));
    await admin.locator(".record-form").waitFor();
    await admin.locator(".record-form [name=partner_id], .record-form [data-field=partner_id], .record-form button[aria-label^='Open Customer']").first().waitFor();
  });
  phase = "edit";
  await admin.goto(url(fixture.contacts,fixture.partner)); await admin.locator(".record-form").waitFor();
  const edit = admin.locator(".status-btn-edit");
  if (await edit.isVisible()) { await edit.focus(); await admin.keyboard.press("e"); }
  const name = admin.locator('.record-form input[name="name"]');
  await name.fill("Measured acceptance port"); await name.press("Tab");
  await admin.waitForFunction(()=>!document.querySelector('input[name="name"]')?.disabled);
  const saved = admin.waitForResponse(r=>r.url().endsWith("/res.partner/web_save") && r.status()===200);
  await name.focus(); await admin.keyboard.press("Alt+s"); await saved;
  evidence.scenarios.push("Administrator edited and saved a contact with the keyboard");
  phase = "preferences";
  await admin.locator(".user-button").click(); await admin.getByRole("menuitem",{name:/my preferences/i}).press("Enter");
  const dialog = admin.locator(".wizard-dialog[open]"), email = dialog.locator("input[type=email]");
  await email.fill("invalid-address");
  await dialog.locator(".field-error:visible").first().waitFor();
  assert.equal(await email.getAttribute("aria-invalid"), "true");
  assert.ok(await dialog.isVisible());
  await email.fill("shell-check@example.test"); await email.press("Tab");
  await admin.waitForFunction(()=>!document.querySelector('.wizard-dialog input[type=email][aria-invalid="true"]'));
  await dialog.getByRole("button",{name:"Update Preferences"}).click();
  await dialog.waitFor({state:"hidden"});
  await admin.locator(".user-button").waitFor();
  evidence.scenarios.push("Invalid email corrected and preferences saved");
  phase = "reader-login";
  const reader = await signIn("shell-reader","fixture-shell-reader",390);
  await reader.goto(url(fixture.contacts,fixture.partner)); await reader.locator(".record-form").waitFor();
  const attrs = await reader.locator("html").evaluate(el=>[el.lang,el.dir]);
  assert.deepEqual(attrs,["ar-001","rtl"]);
  assert.equal(await reader.locator(".status-btn-edit:visible").count(),0);
  const tabs = reader.locator(".record-form [role=tab]"); assert.ok(await tabs.count() >= 2);
  await tabs.first().focus(); await reader.keyboard.press("ArrowLeft");
  assert.equal(await tabs.nth(1).getAttribute("aria-selected"),"true");
  assert.ok(await tabs.nth(1).evaluate(el => el === document.activeElement));
  await audit(reader,"phone read-only contact tabs",".record-form [role=tablist]");
  evidence.scenarios.push("Read-only Arabic session used RTL notebook navigation and focus on phone");
  phase = "scoped-map";
  await reader.goto(url(fixture.map));
  await reader.locator(".stored-map circle").first().waitFor();
  assert.equal(await reader.locator(".stored-map circle").count(),1);
  assert.match(await reader.locator(".stored-map-records").innerText(),/Measured acceptance port/);
  phase = "scoped-activity-grid";
  const described = reader.waitForResponse(reply => reply.url().endsWith("/sale.order/get_views") || reply.url().endsWith("/ir.actions.act_window/erp_load_web_action_views"));
  await reader.goto(url(fixture.activity));
  const activityDescription = await described;
  if (!activityDescription.ok()) {
    const refusal = await activityDescription.json();
    throw new Error(`Activity description returned ${activityDescription.status()}: ${String(refusal.message).slice(0,240)}`);
  }
  await reader.locator(".activity-cell-pill").first().waitFor();
  assert.equal(await reader.locator(".activity-matrix tbody tr").count(),1);
  assert.equal(await reader.locator(".activity-cell-pill").count(),1);
  assert.ok((await reader.locator(".activity-matrix-lead").innerText()).includes(fixture.order_name));
  evidence.scenarios.push("Map shows the permitted contact and the activity grid shows the permitted order, despite another assignment on a restricted order");
  phase = "locale-deactivation";
  const deactivate = await admin.request.post(fixture.origin + "/erp/json/2/res.lang/write", {
    headers: {"Sec-Fetch-Site":"same-origin"}, data: {ids:[fixture.arabic], vals:{active:false}},
  });
  assert.equal(deactivate.status(),200);
  const inactive = await reader.request.post(fixture.origin + "/erp/json/2/res.partner/get_views", {
    headers: {"Sec-Fetch-Site":"same-origin"}, data: {views:[[null,"form"]], wire_major:1, context:{lang:"ar_001"}},
  });
  const inactiveStatus = inactive.status(), inactiveBody = await inactive.text();
  phase = "logout";
  await signOut(admin,303);
  await signOut(reader,500);
  evidence.scenarios.push("Normal and database-refused logout clear the browser session and require sign-in");
  phase = "locale-deactivation";
  assert.equal(inactiveStatus,422);
  assert.match(inactiveBody,/Invalid language code: ar_001/);
  evidence.scenarios.push("RPC accepted the newly activated Arabic locale and refused it immediately after deactivation");
}
const filename = fixture.scenario === "restricted-cards" ? "installed-restricted-cards.json" : "installed-shell.json";
try {
  if (fixture.scenario === "restricted-cards") await checkRestrictedCards();
  else await checkShell();
  evidence.status = "passed";
} catch (err) {
  // Do not persist HTTP payloads, DOM snapshots, cookies or login form input.
  evidence.failed_phase = phase;
  evidence.server_failures = serverFailures;
  console.error(`Installed shell failed during ${phase}: ${err.message.replace(/fixture-(?:administrator-password|shell-reader)/g,"[redacted]").slice(0,900)}`);
  process.exitCode = 1;
} finally {
  await browser.close();
  await writeFile(path.join(output,filename),JSON.stringify(evidence,null,2)+"\n");
  console.log(`Installed shell ${evidence.status}: ${path.join(output,filename)}`);
}

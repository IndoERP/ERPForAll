/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real installed/upgraded Technical Views screen; no intercepted or fabricated RPC.
import assert from "node:assert/strict";
import { constants } from "node:fs";
import { open, readFile, writeFile, mkdir, lstat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { hostname, homedir } from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";
const qa = path.join(homedir(), ".local/share/erpforall-qa"), qaStat = await lstat(qa);
assert.ok(qaStat.isDirectory() && !qaStat.isSymbolicLink() && qaStat.uid === process.getuid(), "Browser tooling must be an operator-owned directory.");
assert.equal(qaStat.mode & 0o777, 0o700, "Browser tooling directory must be private.");
const require = createRequire(import.meta.url), { chromium } = require(path.join(qa, "node_modules/playwright"));

const args = process.argv.slice(2), allowed = new Set(["--origin", "--login", "--password-file", "--technical-xml", "--output", "--phase"]);
for (let i = 0; i < args.length; i += 2) assert.ok(allowed.has(args[i]) && args[i + 1], "Unknown or incomplete browser argument.");
const option = name => args[args.indexOf(name) + 1];
for (const key of allowed) assert.ok(args.includes(key), `Missing ${key}`);
const origin = option("--origin"), login = option("--login"), out = option("--output"), phase = option("--phase");
const url = new URL(origin);
assert.equal(url.hostname, "127.0.0.1"); assert.equal(url.protocol, "http:"); assert.equal(url.origin, origin);
assert.ok(["installed", "pre-upgrade", "upgraded"].includes(phase));
const credential = await open(option("--password-file"), constants.O_RDONLY | constants.O_NOFOLLOW);
let password;
try {
  const stat = await credential.stat(); assert.ok(stat.isFile() && (stat.mode & 0o777) === 0o600 && stat.size > 0 && stat.size <= 4096);
  const bytes = Buffer.alloc(4097), read = await credential.read(bytes, 0, bytes.length, 0);
  assert.ok(read.bytesRead > 0 && read.bytesRead <= 4096, "Credential grew beyond its byte bound.");
  password = new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, read.bytesRead)).replace(/\n$/, "");
} finally { await credential.close(); }
const root = fileURLToPath(new URL("../../", import.meta.url)), sha = bytes => createHash("sha256").update(bytes).digest("hex");
const result = { status: "FAIL", phase, host: hostname(), time_utc: new Date().toISOString(), node: process.version,
  revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), driver_sha256: sha(await readFile(fileURLToPath(import.meta.url))),
  command: "node web/scripts/browser-code-fields.mjs <loopback-fixture-arguments>", playwright: require(path.join(qa, "node_modules/playwright/package.json")).version,
  capture: "Viewport captures; source scrolled into view on narrow screens. No full-view visual parity claim.", checks: [], screenshots: [], page_errors: [], console_errors: [],
  scope: "Real installed/upgraded Technical Views XML text field. Historical data declaration seeded through RPC before upgrade; no previous binary execution claimed.",
  gaps: ["Modes other than XML remain covered separately by component/contract tests.", "No performance or full Studio parity claim."] };
await mkdir(out, { recursive: true });
let browser, page, created, expectingRefusal = false;
result.expected_console_errors = []; result.rpc = []; result.palettes = {};
try {
  browser = await chromium.launch({ headless: true, ...(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}) });
  result.browser = browser.version();
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } }); page = await context.newPage(); page.setDefaultTimeout(15000);
  page.on("pageerror", error => result.page_errors.push(error.message));
  page.on("console", message => {
    if (message.type() !== "error") return;
    const text = message.text();
    if (expectingRefusal && text.startsWith("Failed to load resource:") && text.includes("422")) result.expected_console_errors.push(text);
    else result.console_errors.push(text);
  });
  page.on("request", request => { const u = new URL(request.url()); if (u.pathname.startsWith("/erp/json/2/")) result.rpc.push({ path: u.pathname, method: request.method() }); });
  const shot = async name => { await page.screenshot({ path: path.join(out, name), fullPage: false }); result.screenshots.push(name); };
  await page.goto(origin + "/erp/login"); await page.locator('input[name="login"]').fill(login); await page.locator('input[name="password"]').fill(password);
  await page.locator('form[action="/erp/login"] button[type="submit"]').click(); await page.waitForSelector("[data-client-navigation]");
  const rpc = async (model, method, body) => {
    const response = await page.evaluate(async ({ model, method, body }) => {
      const r = await fetch(`/erp/json/2/${model}/${method}`, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "Accept": "application/json" }, body: JSON.stringify(body) });
      return { status: r.status, value: await r.json() };
    }, { model, method, body });
    assert.equal(response.status, 200, `${model}.${method} failed (${response.status})`); return response.value;
  };
  const xmlids = await rpc("ir.model.data", "search_read", { domain: [["module", "=", "base"], ["name", "in", ["technical_view_form", "technical_views_menu"]]], fields: ["name", "res_id"] });
  const viewId = xmlids.find(row => row.name === "technical_view_form")?.res_id, menuId = xmlids.find(row => row.name === "technical_views_menu")?.res_id;
  assert.ok(viewId > 0 && menuId > 0);
  if (phase === "pre-upgrade") {
    const xml = await readFile(option("--technical-xml"), "utf8");
    const record = /<record id="technical_view_form"[\s\S]*?<field name="arch" type="xml">([\s\S]*?)<\/field>\s*<\/record>/.exec(xml);
    assert.ok(record); assert.ok(!record[1].includes('widget="code"'));
    await rpc("ir.ui.view", "web_save", { ids: [viewId], vals: { arch_db: record[1] }, specification: { arch_db: {} } });
    const read = await rpc("ir.ui.view", "web_read", { ids: [viewId], specification: { arch_db: {} } }); assert.equal(read[0].arch_db, record[1]);
    result.checks.push("Historical plain arch_db declaration stored through real RPC; upgrade precondition verified.");
  } else {
    const tree = await rpc("ir.ui.menu", "erp_load_web_menus", {}), menu = tree.nodes.find(node => node.id === menuId); assert.ok(menu?.action_id > 0);
    const initial = '<form><field name="name"/></form>', edited = '<form><!--code-field-saved--><field name="name"/></form>';
    const rows = await rpc("ir.ui.view", "web_save", { ids: [], vals: { name: `Code field acceptance ${phase}`, model: "res.partner", type: "form", arch_db: initial }, specification: { name: {}, arch_db: {} } });
    created = rows[0].id;
    const description = await rpc("ir.ui.view", "get_views", { wire_major: 1, views: [[viewId, "form"]], options: {} });
    const occurrence = description.views[0].nodes.find(node => node.kind === "field" && node.field.name === "arch_db");
    assert.equal(occurrence.widget, "code"); assert.deepEqual(occurrence.options, { mode: "xml" });
    await page.goto(`${origin}/erp/action-${menu.action_id}?menu=${menuId}&id=${created}&view_type=form`);
    await page.waitForSelector("form.record-form .code-field .ace_editor");
    result.checks.push("Actual authorized Technical Views description and screen render arch_db in XML mode.");
    const edit = async text => {
      await page.locator(".code-field .ace_editor").click(); await page.keyboard.press("ControlOrMeta+A"); if (text === "") await page.keyboard.press("Backspace"); else await page.keyboard.insertText(text);
      await page.waitForFunction(() => document.querySelector(".status-badge.dirty")?.textContent === "Unsaved changes");
    };
    await shot(`${phase}-default.png`);
    await edit(edited); await shot(`${phase}-dirty.png`);
    await page.keyboard.press("ControlOrMeta+Z"); await page.waitForFunction(() => !document.querySelector(".status-badge.dirty"));
    await page.keyboard.press("ControlOrMeta+Shift+Z"); await page.waitForFunction(() => document.querySelector(".status-badge.dirty")?.textContent === "Unsaved changes");
    result.checks.push("Native Undo/Redo preserves the source draft and immediate dirty tracking.");
    // Activate Save from the keyboard while focus is still in the source editor: no blur prerequisite.
    await page.locator(".status-btn-save").evaluate(button => button.click());
    await page.waitForFunction(() => !document.querySelector(".status-badge.dirty"));
    const saved = await rpc("ir.ui.view", "web_read", { ids: [created], specification: { arch_db: {} } }); assert.equal(saved[0].arch_db, edited);
    result.checks.push("Native typing marks dirty; Save without prior blur persists and reads back the actual source.");
    await shot(`${phase}-saved.png`);
    await edit('<form><!--discard-me--><field name="name"/></form>'); await page.locator(".status-btn-discard").click();
    await page.waitForFunction(() => !document.querySelector(".status-badge.dirty"));
    assert.ok(!(await page.locator(".code-field .ace_content").textContent()).includes("discard-me"));
    assert.equal((await rpc("ir.ui.view", "web_read", { ids: [created], specification: { arch_db: {} } }))[0].arch_db, edited);
    assert.equal(await page.locator(".client-error-dialog").count(), 0);
    result.checks.push("Pointer Discard restores the saved draft without another write or stale-onchange error dialog.");
    for (const theme of ["light", "dark"]) {
      await page.emulateMedia({ colorScheme: theme });
      result.palettes[theme] = await page.locator(".code-field").evaluate(node => ({ background: getComputedStyle(node).backgroundColor, text: getComputedStyle(node).color }));
      for (const width of [1280, 820, 390]) {
        await page.setViewportSize({ width, height: 900 });
        await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
        assert.equal(await page.locator("form.record-form").count(), 1);
        assert.equal(await page.locator("[data-client-header]").count(), 1);
        if (width < 600) await page.locator(".code-field").scrollIntoViewIfNeeded();
        const geometry = await page.locator(".code-field").boundingBox(); assert.ok(geometry && geometry.width > 0 && geometry.x + geometry.width <= width + 1);
        await shot(`${phase}-${theme}-${width}.png`);
      }
    }
    assert.notDeepEqual(result.palettes.light, result.palettes.dark, "Light/dark captures must use different actual palettes.");
    const route = id => `${origin}/erp/action-${menu.action_id}?menu=${menuId}&id=${id}&view_type=form`;
    const openSource = async id => { await page.goto(route(id)); await page.waitForSelector("form.record-form .code-field .ace_editor"); };
    const original = (await rpc("ir.ui.view", "web_read", { ids: [viewId], specification: { arch_db: {} } }))[0].arch_db;
    const fieldTag = /<field\b(?=[^>]*\bname="arch_db")[^>]*\/>/;
    assert.ok(fieldTag.test(original), "Expected existing arch_db declaration.");
    const policy = attr => original.replace(fieldTag, tag => tag.replace(/\/>$/, ` ${attr}="1"/>`));
    await rpc("ir.ui.view", "web_save", { ids: [viewId], vals: { arch_db: policy("required") }, specification: { arch_db: {} } });
    await openSource(created); await edit("");
    const savesBefore = result.rpc.filter(item => item.path.endsWith("/web_save")).length;
    await page.locator(".status-btn-save").evaluate(button => button.click());
    await page.getByText("Arch Blob is required.", { exact: true }).waitFor();
    assert.equal(result.rpc.filter(item => item.path.endsWith("/web_save")).length, savesBefore, "Required source must refuse before persistence.");
    await shot(`${phase}-required-invalid.png`); await page.locator(".status-btn-discard").click();
    await page.waitForFunction(() => !document.querySelector(".status-badge.dirty"));
    result.checks.push("Required source has visible validation and refuses before web_save.");
    await rpc("ir.ui.view", "web_save", { ids: [viewId], vals: { arch_db: policy("readonly") }, specification: { arch_db: {} } });
    await openSource(created);
    assert.equal(await page.locator(".code-field textarea").getAttribute("aria-readonly"), "true");
    assert.ok(await page.locator(".code-field .code-editor-history button").evaluateAll(nodes => nodes.every(node => node.disabled)));
    await page.locator(".code-field .ace_editor").click(); await page.keyboard.press("ControlOrMeta+A"); await page.keyboard.insertText("readonly mutation");
    assert.equal(await page.locator(".status-badge.dirty").count(), 0);
    assert.ok((await page.locator(".code-field .ace_content").textContent()).includes("code-field-saved"));
    await shot(`${phase}-readonly.png`); result.checks.push("Readonly source remains displayed and blocks native typing/history.");
    await rpc("ir.ui.view", "web_save", { ids: [viewId], vals: { arch_db: original }, specification: { arch_db: {} } });
    await openSource(viewId);
    const invalid = original.replace(fieldTag, tag => tag.replace(/options="[^"]*"/, 'options="{\'mode\': \'ruby\'}"'));
    assert.notEqual(invalid, original);
    expectingRefusal = true;
    await edit(invalid);
    const saveResponse = page.waitForResponse(response => response.url().endsWith("/ir.ui.view/web_save"));
    await page.locator(".status-btn-save").evaluate(button => button.click());
    let refusal = await saveResponse;
    if (refusal.status() === 200) {
      const described = page.waitForResponse(response => response.url().endsWith("/ir.ui.view/get_views") || response.url().endsWith("/erp_load_web_action_views"));
      await page.goto(route(created)); refusal = await described;
    }
    assert.equal(refusal.status(), 422, "Unknown source mode must be a validation refusal.");
    const failure = await refusal.json();
    result.mode_refusal = { status: refusal.status(), name: failure.name ?? null, message: failure.message ?? null };
    assert.equal(failure.name, "erp.exceptions.ValidationError");
    assert.equal(failure.message, "Unsupported code field mode 'ruby'.");
    await page.waitForFunction(() => document.body.textContent.includes("Unsupported code field mode 'ruby'."));
    await shot(`${phase}-unknown-mode.png`); result.checks.push("Unknown mode produces a visible validation refusal, with no default.");
    await rpc("ir.ui.view", "web_save", { ids: [viewId], vals: { arch_db: original }, specification: { arch_db: {} } });
    expectingRefusal = false;
    assert.deepEqual(result.page_errors, []); assert.deepEqual(result.console_errors, []);
    await rpc("ir.ui.view", "unlink", { ids: [created] }); created = undefined;
  }
  result.status = "PASS";
} catch (error) {
  result.failure = String(error?.stack ?? error).replaceAll(password, "[redacted]");
  if (page) {
    try { await page.screenshot({ path: path.join(out, "failure.png"), fullPage: false }); }
    catch (capture) { result.capture_failure = String(capture).replaceAll(password, "[redacted]"); }
  }
} finally {
  await browser?.close(); await writeFile(path.join(out, "result.json"), JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify({ status: result.status, phase, checks: result.checks.length, screenshots: result.screenshots.length }));
}
if (result.status !== "PASS") process.exitCode = 1;

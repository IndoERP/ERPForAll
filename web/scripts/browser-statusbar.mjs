/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual compiled client with explicit HTTP fixture responses, not backend/auth/database acceptance.
import assert from "node:assert/strict";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { hostname } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { createRequire } from "node:module";
const qaRequire = createRequire(path.join(process.env.HOME, ".local/share/erpforall-qa/package.json"));
const { chromium } = qaRequire("playwright");
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.CLIENT_BROWSER_OUTPUT;
if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to an isolated output directory.");
await mkdir(output, { recursive: true });
const manifest = JSON.parse(await readFile(path.join(root, "web/dist/human-ui/manifest.json"), "utf8"));
const assets = new Map();
for (const asset of manifest.assets) {
  const data = await readFile(path.join(root, "web/dist/human-ui", asset.path));
  assert.equal(createHash("sha256").update(data).digest("hex"), asset.sha256);
  assets.set("/erp/assets/" + asset.path, { data, type: asset.content_type });
}
for (const name of ["logo.svg", "logo-dark.svg"]) assets.set("/erp/brand/" + name, { data: await readFile(path.join(root, "crates/erp-server/brand/erpforall-" + name)), type: "image/svg+xml" });
assets.set("/theme_office_blue/static/src/theme.css", { data: await readFile(path.join(root, "addons/theme_office_blue/static/src/theme.css")), type: "text/css; charset=utf-8" });
const js = manifest.assets.find(asset => asset.name === "client.js"), css = manifest.assets.find(asset => asset.name === "client.css");
const fields = { name: { type: "char", label: "Name", readonly: false, required: true, selection: null, relation: null, relation_field: null }, phase: { type: "selection", label: "Phase", readonly: false, required: false, selection: [["draft", "Draft"], ["review", "Review"], ["done", "Done"]], relation: null, relation_field: null } };
const field = (name, widget = fields[name].type, options = {}) => ({ kind: "field", field: { model: "test.note", name }, widget, label: fields[name].label, options, force_save: false, modifiers: { readonly: null, required: null, invisible: null } });
const description = { wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields: { "test.note": fields }, notices: [], views: [
  { identity: { kind: "stored", id: 1 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field("name")] },
  { identity: { kind: "stored", id: 2 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1, 3] }, { kind: "header", children: [2] }, field("phase", "statusbar", { visible: ["draft", "done"] }), { kind: "sheet", children: [4] }, field("name")] }
] };
const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "fixture-a1", identity: { uid: 2, name: "Fixture user", internal: true }, companies: [{ id: 1, name: "Fixture company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "fixture-not-secret" } };
const calls = [], errors = [], checks = [], screenshots = [];
let serverError;
let persisted = { id: 5, name: "Statusbar record", phase: "review" };
const server = createServer(async (req, res) => {
  try {
    const pathname = new URL(req.url, "http://fixture").pathname, asset = assets.get(pathname);
    if (asset) { res.setHeader("Content-Type", asset.type); res.end(asset.data); return; }
    if (req.method === "GET" && ["/erp", "/erp/action-20"].includes(pathname)) {
      res.setHeader("Content-Type", "text/html"); res.end(`<!doctype html><html lang="en"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Statusbar fixture</title><div id="erp-root"></div><script type="application/json" id="erp-bootstrap">${JSON.stringify(bootstrap)}</script><script type="module" src="/erp/assets/${js.path}"></script></html>`); return;
    }
    if (pathname === "/favicon.ico") { res.writeHead(204); res.end(); return; }
    let input = ""; for await (const chunk of req) { input += chunk; if (input.length > 32768) throw new Error("Oversized fixture request"); }
    const body = JSON.parse(input); calls.push({ path: pathname, body }); let value;
    if (pathname.endsWith("erp_describe")) value = { wire_major: 1, registry_version: "r1", scope: body.scope, declarations: [{ key: "theme_office_blue.appearance", kind: "selection", label: "Appearance", choices: [["default", "Default"], ["office-blue", "Office Blue"]], writable: true }] };
    else if (pathname.endsWith("erp_read_values")) value = { wire_major: 1, scope: body.scope, revision: "1", values: [{ key: "theme_office_blue.appearance", value: "office-blue", source: "stored" }] };
    else if (pathname.endsWith("erp_load_web_menus")) value = { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] };
    else if ((pathname.endsWith("erp_load_web_action") || pathname.endsWith("erp_load_web_action_views"))) value = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "list" }, { id: 2, type: "form" }], domain: null, context: null };
    else if (pathname.endsWith("get_views")) value = description;
    else if (pathname.endsWith("web_search_read")) value = { length: 1, records: [persisted] };
    else if (pathname.endsWith("search_count")) value = 1;
    else if (pathname.endsWith("web_read")) value = [persisted];
    else if (pathname.endsWith("default_get")) value = { name: "New record", phase: "draft" };
    else if (pathname.endsWith("onchange")) value = { value: {} };
    else if (pathname.endsWith("web_save")) { persisted = { ...persisted, ...body.vals }; value = [persisted]; }
    else throw new Error(`Unknown fixture RPC: ${pathname}`);
    res.setHeader("Content-Type", "application/json"); res.end(JSON.stringify(pathname.endsWith("erp_load_web_action_views") && res.statusCode === 200 ? { action: value, views: description } : value));
  } catch (error) { serverError = error; res.writeHead(500, { "Content-Type": "application/json" }); res.end(JSON.stringify({ message: "Fixture server failure" })); }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
const endpoint = process.env.MOLI_CDP_URL;
if (!endpoint) throw new Error("Set MOLI_CDP_URL to an isolated Moli CDP server with --layout.");
const browser = await chromium.connectOverCDP(endpoint);
const result = { status: "FAIL", scope: "compiled client assets with explicit HTTP fixtures; not backend/auth/database acceptance", host: hostname(), time_utc: new Date().toISOString(), node: process.version, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), browser: browser.version(), browser_transport: "Moli CDP", assets: manifest.assets, checks, screenshots };
let activePage;
try {
  const page = await browser.newPage(); activePage = page; page.on("pageerror", error => errors.push(error.message));
  for (const width of [1440, 820, 390]) {
    await page.setViewportSize({ width, height: 900 });
    persisted.phase = "review";
    await page.goto(origin + "/erp/action-20?id=5&view_type=form");
    const bar = page.locator(".form-header .form-statusbar"); await bar.waitFor();
    assert.deepEqual(await bar.locator("button").allTextContents(), ["Draft", "Review", "Done"]);
    assert.equal(await bar.locator('[aria-current="step"]').textContent(), "Review");
    const before = calls.filter(call => call.path.endsWith("web_save")).length;
    await bar.getByRole("button", { name: "Done", exact: true }).click();
    await page.waitForFunction(() => document.querySelector('.form-statusbar [aria-current="step"]')?.textContent === "Done");
    assert.equal(calls.filter(call => call.path.endsWith("web_save")).length, before, "Click edits draft without implicit write");
    assert.ok(calls.findLast(call => call.path.endsWith("onchange")), "Normal onchange path used");
    await page.screenshot({ path: path.join(output, `before-save-${width}.png`), fullPage: true });
    result.save_geometry = await page.locator(".status-btn-save").evaluate(node => ({ rect: node.getBoundingClientRect().toJSON(), display: getComputedStyle(node).display, visibility: getComputedStyle(node).visibility, html: node.outerHTML, parent: node.parentElement?.outerHTML }));
    await page.getByRole("button", { name: "Save", exact: true }).click({ timeout: 8000 });
    const saveDeadline = Date.now() + 15000;
    while (calls.filter(call => call.path.endsWith("web_save")).length === before) {
      if (Date.now() >= saveDeadline) throw new Error("Save request did not reach fixture.");
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    assert.equal(calls.findLast(call => call.path.endsWith("web_save")).body.vals.phase, "done");
    assert.equal(persisted.phase, "done");
    await page.reload(); await bar.waitFor();
    assert.equal(await bar.locator('[aria-current="step"]').textContent(), "Done");
    assert.deepEqual(await bar.locator("button").allTextContents(), ["Draft", "Done"]);
    const box = await bar.boundingBox(); assert.ok(box && box.x >= 0 && box.x + box.width <= width, "Statusbar fits viewport");
    const screenshot = `statusbar-${width}.png`; await page.screenshot({ path: path.join(output, screenshot), fullPage: true }); screenshots.push(screenshot);
    checks.push({ width, actual_state_and_labels: "PASS", current_outside_filter: "PASS", normal_onchange: "PASS", no_implicit_write: "PASS", save_reload: "PASS", viewport: "PASS" });
  }
  if (serverError) throw serverError;
  assert.deepEqual(errors, []); result.status = "PASS";
} catch (error) { result.error = error instanceof Error ? error.stack : String(error); if (activePage) await activePage.screenshot({ path: path.join(output, "failure.png"), fullPage: true }); process.exitCode = 1; }
finally { await writeFile(path.join(output, "statusbar.json"), JSON.stringify(result, null, 2) + "\n"); await browser.close(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }

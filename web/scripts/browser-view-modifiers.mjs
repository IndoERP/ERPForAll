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
const js = manifest.assets.find(asset => asset.name === "client.js"), css = manifest.assets.find(asset => asset.name === "client.css");
const literal = value => ({ schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value } }] });
const fields = Object.fromEntries(["name", "computed", "hidden", "optional"].map(name => [name, { type: "char", label: name, readonly: name === "computed", required: name === "optional" || name === "hidden", selection: null, relation: null, relation_field: null }]));
const field = (name, modifiers = {}, force_save = false) => ({ kind: "field", field: { model: "test.note", name }, widget: "char", label: name, options: {}, force_save, modifiers: { readonly: null, required: null, invisible: null, ...modifiers } });
const description = { wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields: { "test.note": fields }, notices: [], views: [
  { identity: { kind: "stored", id: 1 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1, 2] }, field("name"), field("hidden", { invisible: literal(true) })] },
  { identity: { kind: "stored", id: 2 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1, 2, 3, 4] }, field("name"), field("computed", {}, true), field("hidden", { invisible: literal(true) }), field("optional", { required: literal(false) })] }
] };
const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "fixture-a1", identity: { uid: 2, name: "Fixture user", internal: true }, companies: [{ id: 1, name: "Fixture company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "fixture-not-secret" } };
const calls = [], errors = [], checks = [], screenshots = [];
let serverError;
let persisted = { id: 5, name: "Before", computed: "Original", hidden: "", optional: "" };
const server = createServer(async (req, res) => {
  try {
    const pathname = new URL(req.url, "http://fixture").pathname, asset = assets.get(pathname);
    if (asset) { res.setHeader("Content-Type", asset.type); res.end(asset.data); return; }
    if (req.method === "GET" && ["/erp", "/erp/action-20"].includes(pathname)) {
      res.setHeader("Content-Type", "text/html"); res.end(`<!doctype html><html lang="en"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Modifiers fixture</title><div id="erp-root"></div><script type="application/json" id="erp-bootstrap">${JSON.stringify(bootstrap)}</script><script type="module" src="/erp/assets/${js.path}"></script></html>`); return;
    }
    if (pathname === "/favicon.ico") { res.writeHead(204); res.end(); return; }
    let input = ""; for await (const chunk of req) { input += chunk; if (input.length > 32768) throw new Error("Oversized fixture request"); }
    const body = JSON.parse(input); calls.push({ path: pathname, body }); let value;
    if (pathname.endsWith("erp_load_web_menus")) value = { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] };
    else if ((pathname.endsWith("erp_load_web_action") || pathname.endsWith("erp_load_web_action_views"))) value = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "list" }, { id: 2, type: "form" }], domain: null, context: null };
    else if (pathname.endsWith("get_views")) value = description;
    else if (pathname.endsWith("web_search_read")) value = { length: 1, records: [persisted] };
    else if (pathname.endsWith("search_count")) value = 1;
    else if (pathname.endsWith("web_read")) value = [persisted];
    else if (pathname.endsWith("default_get")) value = { name: "Draft", computed: "Default", hidden: "", optional: "" };
    else if (pathname.endsWith("onchange")) value = { value: { computed: "Calculated", hidden: "Kept" } };
    else if (pathname.endsWith("web_save")) { persisted = { ...persisted, ...body.vals }; value = [persisted]; }
    else throw new Error(`Unknown fixture RPC: ${pathname}`);
    res.setHeader("Content-Type", "application/json"); res.end(JSON.stringify(pathname.endsWith("erp_load_web_action_views") && res.statusCode === 200 ? { action: value, views: description } : value));
  } catch (error) { serverError = error; res.writeHead(500, { "Content-Type": "application/json" }); res.end(JSON.stringify({ message: "Fixture server failure" })); }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", headless: true });
const result = { status: "FAIL", scope: "compiled client assets with explicit HTTP fixtures; not backend/auth/database acceptance", host: hostname(), time_utc: new Date().toISOString(), node: process.version, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), browser: browser.version(), assets: manifest.assets, checks, screenshots };
try {
  const page = await browser.newPage(); page.on("pageerror", error => errors.push(error.message));
  for (const width of [1440, 820, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto(origin + "/erp/action-20?view_type=list");
    await page.getByRole("columnheader", { name: "name", exact: false }).waitFor();
    assert.equal(await page.getByRole("columnheader", { name: "hidden", exact: false }).count(), 0);
    await page.goto(origin + "/erp/action-20?id=5&view_type=form");
    const name = page.locator('input[name="name"]'); await name.waitFor();
    assert.equal(await page.locator('[name="hidden"]').count(), 0);
    assert.equal(await page.locator('input[name="optional"]').getAttribute("required"), null);
    assert.equal(await page.locator('input[name="computed"]:not([readonly]):not([disabled])').count(), 0);
    await name.fill("Updated " + width); await name.press("Tab");
    const saved = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("web_save") && response.ok());
    await page.getByRole("button", { name: "Save", exact: true }).click();
    await saved;
    const save = calls.findLast(call => call.path.endsWith("web_save"));
    assert.ok(save, "Save request reached fixture");
    assert.deepEqual(save.body.vals, { name: "Updated " + width, computed: "Calculated", hidden: "Kept" });
    const screenshot = `modifiers-${width}.png`; await page.screenshot({ path: path.join(output, screenshot), fullPage: true }); screenshots.push(screenshot);
    checks.push({ width, hidden_column: "PASS", hidden_control: "PASS", optional_required_override: "PASS", readonly_control: "PASS", forced_onchange_save: "PASS" });
  }
  if (serverError) throw serverError;
  assert.deepEqual(errors, []); result.status = "PASS";
} catch (error) { result.error = error instanceof Error ? error.stack : String(error); process.exitCode = 1; }
finally { await browser.close(); await new Promise(resolve => server.close(resolve)); await writeFile(path.join(output, "view-modifiers.json"), JSON.stringify(result, null, 2) + "\n"); }

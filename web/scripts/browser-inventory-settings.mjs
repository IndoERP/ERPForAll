/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual installed settings descriptions rendered by the real component; RPC mutations are fixture-only.
import assert from "node:assert/strict";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { hostname } from "node:os";
import { createServer } from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { chromium } from "playwright";
import { auditPage } from "../testing/axe.ts";
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.INVENTORY_SETTINGS_OUTPUT;
const source = process.env.INVENTORY_SETTINGS_FIXTURE;
assert.ok(output && source, "Set INVENTORY_SETTINGS_OUTPUT and INVENTORY_SETTINGS_FIXTURE.");
const fixture = JSON.parse(await readFile(source, "utf8"));
for (const scope of [fixture.company, fixture.global]) {
  scope.description.declarations = scope.description.declarations.filter(d => d.key.startsWith("stock."));
  scope.description.items = scope.description.items.filter(d => d.key.startsWith("stock."));
}
const company = fixture.company.description.scope.company_id;
assert.equal(fixture.company.description.declarations.length, 7);
assert.equal(fixture.global.description.declarations.length, 1);
await mkdir(output, { recursive: true });
const entry = `import { settingsView } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/services.ts"))};
import { RpcClient } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/transport.ts"))};
const fixture=${JSON.stringify(fixture)};
const calls=[];const targets=[];
const rpc=new RpcClient(async(path,init)=>{
 const body=JSON.parse(init.body);const method=String(path).split('/').at(-1);calls.push({method,body});
 const scope=body.scope.kind==='company'?fixture.company:fixture.global;
 const value=method==='erp_describe'?scope.description:scope.values;
 return new Response(JSON.stringify(value),{headers:{'Content-Type':'application/json'}});
},()=>{});
const jobs=[];
const controller=await settingsView(document.querySelector('#host'),rpc,[{id:${company},name:'Inventory fixture company'}],job=>{const promise=job();jobs.push(promise);promise.catch(error=>{window.fixtureError=error.message;});},()=>{},{},['global','company'],undefined,undefined,undefined,'stock',
 {navigation:{roots:[{id:1,module:'stock',label:'Inventory',icon:'lucide:package'}],openTarget:async target=>{targets.push(target);}}});
window.fixture={controller,calls,targets,company:${company}};window.fixtureReady=true;`;
const bundle = await build({ stdin: { contents: entry, resolveDir: path.join(root, "web"), sourcefile: "inventory-settings-fixture.ts", loader: "ts" }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023" });
const css = await readFile(path.join(root, "addons/web/static/src/client/client.css"));
const server = createServer((request, response) => {
  if (request.url === "/fixture.js") { response.setHeader("Content-Type", "text/javascript"); response.end(bundle.outputFiles[0].contents); return; }
  if (request.url === "/client.css") { response.setHeader("Content-Type", "text/css"); response.end(css); return; }
  const theme = new URL(request.url, "http://127.0.0.1").searchParams.get("theme");
  if (!["light", "dark"].includes(theme)) { response.writeHead(400); response.end(); return; }
  response.setHeader("Content-Type", "text/html; charset=utf-8");
  response.end(`<!doctype html><html lang="en" data-theme="${theme}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Inventory settings rendering fixture</title><link rel="stylesheet" href="/client.css"></head><body><main><h1>Inventory settings</h1><div id="host"></div></main><script type="module" src="/fixture.js"></script></body></html>`);
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
let browser;
const errors = [], captures = [];
try {
  browser = await chromium.launch({ headless: true, ...(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}) });
  const page = await browser.newPage();
  page.on("pageerror", error => errors.push(error.message));
  for (const theme of ["light", "dark"]) for (const width of [1440, 800, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await page.emulateMedia({ colorScheme: theme, reducedMotion: "reduce" });
    await page.goto(`http://127.0.0.1:${server.address().port}/?theme=${theme}`);
    await page.waitForFunction(() => window.fixtureReady === true);
    await page.locator(`[data-scope="${company}"]`).focus();
    await page.keyboard.press("Enter");
    await page.locator('[data-setting-key="stock.picking_policy"]').waitFor();
    assert.match(await page.locator('[data-setting-key="stock.group_stock_multi_locations"]').textContent(), /a later release/);
    assert.equal(await page.locator('[data-setting-key="stock.group_stock_multi_locations"] .settings-info-value').textContent(), "Multiple locations (unavailable)");
    assert.equal(await page.locator('[data-setting-key="stock.group_stock_multi_locations"] input').count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await auditPage(page);
    let file = `inventory-company-${theme}-${width}.png`;
    await page.screenshot({ path: path.join(output, file), fullPage: true }); captures.push(file);
    await page.locator('[data-setting-key="stock.stock_move_email_validation"] input').check();
    assert.equal(await page.evaluate(() => window.fixture.controller.dirty()), true);
    await page.evaluate(() => window.fixture.controller.discard());
    await page.waitForFunction(() => !window.fixture.controller.busy());
    assert.equal(await page.evaluate(() => window.fixture.controller.dirty()), false);
    await page.locator('[data-setting-key="stock.module_delivery"] button').click();
    await page.waitForFunction(() => window.fixture.targets.length === 1);
    assert.equal(await page.evaluate(() => window.fixture.calls.some(c => c.method === 'button_immediate_install')), false);
    await page.locator('[data-scope="global"]').click();
    await page.locator('[data-setting-key="stock.barcode_separator"]').waitFor();
    await auditPage(page);
    file = `inventory-global-${theme}-${width}.png`;
    await page.screenshot({ path: path.join(output, file), fullPage: true }); captures.push(file);
    assert.equal(await page.evaluate(() => window.fixtureError), undefined);
  }
  assert.deepEqual(errors, []);
  const result = { host: hostname(), utc: new Date().toISOString(), command: process.argv, node: process.version, browser: browser.version(), sourceRevision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), captures, errors, scope: "Installed backend description and values rendered in the real Settings component. Navigation callback and reload responses are fixtures; live shell, login and deployment are not tested here." };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2));
  console.log(JSON.stringify(result));
} finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }

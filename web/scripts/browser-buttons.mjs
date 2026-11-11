/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual shipped client helpers mounted in an isolated compiled component fixture.
// This supplies no installed-addon, backend, authentication or deployment acceptance.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { hostname } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { connectCaptureBrowser, runStateCaptures, finishCapture, preparePrivateOutputDirectory } from "./browser-erp-ui-states.mjs";
import { auditPage } from "../testing/axe.ts";

const root = fileURLToPath(new URL("../../", import.meta.url));
const RANKS = ["primary", "secondary", "link", "success", "info", "warning", "danger"];
const viewports = [
  { name: "desktop", width: 1440, height: 1000 },
  { name: "intermediate", width: 820, height: 1000 },
  { name: "phone", width: 390, height: 1000 },
];
const states = ["default", ...["hover", "focus", "pressed"].flatMap(state => RANKS.map(rank => `${state}-${rank}`)), "disabled-pending", "confirm", "dropdown", "header-actions", "smart-overflow"];
function selected(name, allowed) {
  if (process.env[name] === undefined) return allowed;
  const values = process.env[name].split(",");
  assert.ok(values.length && values.every(value => allowed.includes(value)) && new Set(values).size === values.length, `Invalid ${name}`);
  return values;
}

function fixtureSource() {
  return `import { createButtonControl } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/button-control.ts"))};
import { createButtonMenu } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/button-menu.ts"))};
import { renderFormLayout } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/form-layout.ts"))};
import { choose } from ${JSON.stringify(path.join(root, "addons/web/static/src/client/dom.ts"))};
const log = [];
const host = document.querySelector('main');
const section = (title, id) => { const root=document.createElement('section'); root.id=id; const heading=document.createElement('h2'); heading.textContent=title; root.append(heading); host.append(root); return root; };
const row = (parent) => { const node=document.createElement('div'); node.className='fixture-row'; parent.append(node); return node; };
const sentinel=document.createElement('button'); sentinel.id='focus-start'; sentinel.type='button'; sentinel.textContent='Start keyboard route'; host.append(sentinel);
const ranks=section('Action ranks','ranks'); const rankRow=row(ranks);
for(const rank of ${JSON.stringify(RANKS)}) { const control=createButtonControl({label:rank,rank,action:()=>log.push(rank)}); control.id='rank-'+rank; rankRow.append(control); }
const disabled=section('Unavailable controls','disabled');
for(const mode of ['disabled','pending']) { const group=row(disabled); for(const rank of ${JSON.stringify(RANKS)}) { const control=createButtonControl({label:rank+' '+mode,rank,action:()=>log.push('unexpected-'+mode),[mode]:true}); control.dataset.state=mode; group.append(control); } }
const shapes=section('Size and icon presentation','shapes'); const shapeRow=row(shapes);
for(const size of ['small','normal','large']) shapeRow.append(createButtonControl({label:size,size,action:()=>log.push(size)}));
shapeRow.append(createButtonControl({label:'',accessibleLabel:'Refresh record',icon:'refresh-cw',circle:true,action:()=>log.push('circle')}));
const confirm=createButtonControl({label:'Confirm action',action:()=>{ void choose('Confirm action','Run this fixture action?',['Continue','Cancel']).then(value=>log.push('confirm:'+value)); }}); confirm.id='confirm-action'; shapeRow.append(confirm);
const menuSection=section('Dropdown interaction','dropdown'); const dropdown=row(menuSection); dropdown.classList.add('button-overflow');
const opener=createButtonControl({label:'Open component menu',action:()=>{}}); opener.id='component-menu-opener'; dropdown.append(opener);
const menu=document.createElement('div'); menu.id='component-menu'; dropdown.append(menu);
const items=['First choice','Unavailable choice','Last choice'].map((label,index)=>createButtonControl({label,disabled:index===1,action:()=>log.push(label)})); menu.append(...items);
const menuHandle=createButtonMenu({opener,menu,items:()=>items});
const formSection=section('Declared record action layout','declared-form');
const form=document.createElement('form'); form.noValidate=true; formSection.append(form);
const action=(name,label,kind='button')=>({kind,name,label,variant:'secondary',confirm:null,invisible:null,...(kind==='stat_button'?{children:[]}: {})});
const nodes=[{kind:'root',children:[{kind:'header',children:[action('hidden','Hidden action'),action('approve','Approve'),action('export','Export'),action('void','Void'),action('archive','Archive')]},{kind:'sheet',children:[{kind:'button_box',children:Array.from({length:9},(_,i)=>action('related_'+i,'Related '+(i+1),'stat_button'))}]}]}];
const layout=renderFormLayout(form,nodes,()=>{},new Map(),(node,parent)=>{if(node.name==='hidden')return; const control=createButtonControl({label:node.label,rank:node.variant,action:()=>log.push(node.name)}); control.dataset.probe=node.name; parent.append(control); return control;});
window.buttonFixture={log,menuHandle,dispose(){menuHandle.dispose();layout.dispose();}};
window.fixtureReady=true;`;
}

async function main() {
  const captureStates = selected("BUTTON_CAPTURE_STATES", states);
  const captureAppearances = selected("BUTTON_CAPTURE_APPEARANCES", ["light", "dark"]);
  const captureViewports = selected("BUTTON_CAPTURE_VIEWPORTS", viewports.map(viewport => viewport.name)).map(name => viewports.find(viewport => viewport.name === name));
  const output = process.env.CLIENT_BROWSER_OUTPUT;
  if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to an isolated output directory.");
  const endpoint = process.env.MOLI_CDP_URL;
  await preparePrivateOutputDirectory(output);
  let browser, server, activePage;
  const checks = [], browserErrors = [];
  const result = {
    status: "FAIL", scope: "compiled shipped component helpers; not installed-addon/backend/auth/database acceptance",
    host: hostname(), time_utc: new Date().toISOString(), node: process.version,
    command: process.argv, source_revision: null, browser_transport: endpoint ? "Moli CDP" : "Playwright Chromium", checks,
  };
  try {
    try { result.source_revision = (await readFile(path.join(root, "REVISION"), "utf8")).trim(); }
    catch (error) { if (error.code !== "ENOENT") throw error; result.source_revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim(); }
    const source = fixtureSource();
    const bundled = await build({ stdin: { contents: source, resolveDir: root, sourcefile: "button-component-fixture.js", loader: "js" }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023" });
    assert.equal(bundled.outputFiles.length, 1);
    const js = bundled.outputFiles[0].contents;
    const css = await readFile(path.join(root, "addons/web/static/src/client/client.css"));
    result.assets = { js_sha256: createHash("sha256").update(js).digest("hex"), css_sha256: createHash("sha256").update(css).digest("hex") };
    await writeFile(path.join(output, "fixture-entry.js"), source, { mode: 0o600 });
    server = createServer((request, response) => {
      const target = new URL(request.url, "http://fixture");
      response.setHeader("Cache-Control", "no-store");
      if (target.pathname === "/fixture.js") { response.setHeader("Content-Type", "text/javascript; charset=utf-8"); response.end(js); return; }
      if (target.pathname === "/client.css") { response.setHeader("Content-Type", "text/css; charset=utf-8"); response.end(css); return; }
      if (target.pathname === "/favicon.ico") { response.writeHead(204); response.end(); return; }
      if (target.pathname !== "/") { response.writeHead(404); response.end(); return; }
      const appearance = target.searchParams.get("appearance");
      if (appearance !== "light" && appearance !== "dark") { response.writeHead(400); response.end("Invalid appearance"); return; }
      response.setHeader("Content-Type", "text/html; charset=utf-8");
      response.end(`<!doctype html><html lang="en" data-theme="${appearance}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Button component fixture</title><link rel="stylesheet" href="/client.css"><style>main{max-width:1200px;margin:0 auto;padding:24px;box-sizing:border-box}section{margin-block:24px}h1{font-size:24px}h2{font-size:17px}.fixture-row{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-block:8px}.button-overflow{position:relative}#component-menu{min-width:180px}</style></head><body><main><h1>Button component fixture</h1><p>Actual client helpers with explicit component data.</p></main><script type="module" src="/fixture.js"></script></body></html>`);
    });
    await new Promise((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
    const origin = `http://127.0.0.1:${server.address().port}`;
    const qaRequire = createRequire(path.join(process.env.HOME, ".local/share/erpforall-qa/package.json"));
    const { chromium } = qaRequire("playwright");
    browser = await connectCaptureBrowser(chromium, { moliCdpUrl: endpoint, executablePath: process.env.CHROME_BIN });
    result.browser = browser.version();
    const page = await browser.newPage(); activePage = page;
    page.setDefaultTimeout(5000);
    page.on("pageerror", error => browserErrors.push(error.message));
    const capture = await runStateCaptures({
      states: captureStates, appearances: captureAppearances, viewports: captureViewports,
      setViewport: viewport => page.setViewportSize({ width: viewport.width, height: viewport.height }),
      prepareState: async (state, { appearance, viewport }) => {
        await page.mouse.up();
        await page.keyboard.up("Space");
        await page.goto(`${origin}/?appearance=${appearance}`);
        await page.waitForFunction(() => window.fixtureReady === true);
        const rank = state.split("-").slice(1).join("-");
        if (state.startsWith("hover-")) { await page.locator(`#rank-${rank}`).hover(); assert.equal(await page.locator(`#rank-${rank}`).evaluate(node => node.matches(":hover")), true); }
        else if (state.startsWith("focus-")) {
          await page.locator("#focus-start").focus();
          for (let index = 0; index <= RANKS.indexOf(rank); index++) await page.keyboard.press("Tab");
          assert.equal(await page.evaluate(() => document.activeElement?.id), `rank-${rank}`);
          assert.equal(await page.locator(`#rank-${rank}`).evaluate(node => node.matches(":focus-visible")), true);
        } else if (state.startsWith("pressed-")) {
          const target = page.locator(`#rank-${rank}`); await target.focus(); await page.keyboard.down("Space");
          const pressed = await target.evaluate(node => ({ active: node.matches(":active"), focused: document.activeElement === node, background: getComputedStyle(node).backgroundColor, box: node.getBoundingClientRect().toJSON() }));
          assert.equal(pressed.active, true, JSON.stringify({input: "keyboard Space held", ...pressed}));
        } else if (state === "disabled-pending") {
          assert.equal(await page.locator('#disabled button:disabled').count(), RANKS.length * 2);
          assert.deepEqual(await page.evaluate(() => window.buttonFixture.log), []);
        } else if (state === "confirm") {
          await page.locator("#confirm-action").focus(); await page.keyboard.press("Enter"); await page.getByRole("dialog").waitFor({state: "attached"});
          const modal = await page.getByRole("dialog").evaluate(node => ({ open: node.open, display: getComputedStyle(node).display, box: node.getBoundingClientRect().toJSON() }));
          await page.screenshot({path: path.join(output, `diagnostic-confirm-${appearance}-${viewport.name}.png`), fullPage:true});
          assert.ok(modal.open && modal.display !== "none" && modal.box.width > 0 && modal.box.height > 0, JSON.stringify(modal));
          assert.equal(await page.getByRole("dialog").getByRole("button", { name: "Continue", exact: true }).evaluate(node => node === document.activeElement), true);
        } else if (state === "dropdown") {
          const opener = page.locator("#component-menu-opener"); await opener.focus(); await page.keyboard.press("ArrowDown");
          assert.equal(await opener.getAttribute("aria-expanded"), "true");
          await page.keyboard.press("End"); assert.equal(await page.evaluate(() => document.activeElement?.textContent), "Last choice");
          await page.keyboard.press("Home"); assert.equal(await page.evaluate(() => document.activeElement?.textContent), "First choice");
          await page.keyboard.press("Tab"); assert.equal(await page.evaluate(() => document.activeElement?.textContent), "Last choice");
          await page.keyboard.press("Shift+Tab"); assert.equal(await page.evaluate(() => document.activeElement?.textContent), "First choice");
        } else if (state === "header-actions") {
          const group = page.locator(".form-header-actions");
          assert.equal(await group.locator('[data-probe="hidden"]').count(), 0);
          assert.equal(await group.locator("[data-probe]").count(), 4);
          const opener = group.getByRole("button", { name: "More actions", exact: true });
          assert.equal(await opener.count(), viewport.width <= 700 ? 1 : 0);
          if (viewport.width <= 700) {
            assert.equal(await group.locator(':scope > [data-probe="approve"]').count(), 1);
            await opener.focus(); await page.keyboard.press("Enter"); assert.equal(await opener.getAttribute("aria-expanded"), "true");
            assert.deepEqual(await page.evaluate(() => window.buttonFixture.log), []);
          }
        } else if (state === "smart-overflow") {
          const group = page.locator(".form-button-box");
          assert.equal(await group.locator("[data-probe]").count(), 9);
          const opener = group.getByRole("button", { name: "More", exact: true }); await opener.focus(); await page.keyboard.press("Enter");
          assert.equal(await opener.getAttribute("aria-expanded"), "true");
          const names = await group.locator('[role="menu"] [data-probe]').allTextContents(); assert.ok(names.length > 0);
          assert.equal(names.at(-1), "Related 9");
        }
        const geometry = await page.locator('button:not(:disabled)').evaluateAll(nodes => nodes.filter(node => !node.closest('[hidden]') && getComputedStyle(node).display !== 'none' && node.getBoundingClientRect().width > 0).map(node => ({ text: node.getAttribute('aria-label') || node.textContent, x: node.getBoundingClientRect().x, width: node.getBoundingClientRect().width, height: node.getBoundingClientRect().height })));
        for (const control of geometry) { assert.ok(control.width >= 24 && control.height >= 24, `Readable hit target: ${control.text}`); assert.ok(control.x >= -1 && control.x + control.width <= viewport.width + 1, `Control fits viewport: ${control.text}`); }
        const openMenus = await page.locator('[role="menu"]:not([hidden])').evaluateAll(nodes => nodes.map(node => ({rect: node.getBoundingClientRect().toJSON(), overflow: getComputedStyle(node).overflowY})));
        for (const menu of openMenus) {
          assert.ok(menu.rect.width > 0 && menu.rect.height > 0, `Open menu has rendered geometry: ${JSON.stringify(menu)}`);
          assert.ok(menu.rect.y >= 0 && menu.rect.bottom <= viewport.height + 1, `Menu fits viewport vertically: ${JSON.stringify(menu)}`);
          assert.equal(menu.overflow, "auto");
        }
        checks.push({ state, appearance, viewport: viewport.name, status: "PASS", geometry, openMenus });
      },
      captureState: async (_state, { file }) => {
        const destination = path.join(output, file); await mkdir(path.dirname(destination), { recursive: true, mode: 0o700 });
        await page.screenshot({ path: destination, fullPage: true });
      },
      auditState: async (state, { appearance, viewport }) => {
        const audit = await auditPage(page);
        const destination = path.join(output, appearance, viewport.name, `${state}.axe.json`);
        await writeFile(destination, JSON.stringify(audit, null, 2) + "\n", { mode: 0o600 });
      },
    });
    await page.mouse.up();
    assert.deepEqual(browserErrors, [], "No component page errors");
    const { manifest } = await finishCapture(output, { generatedAt: result.time_utc, origin, browserVersion: result.browser, appearances: captureAppearances, viewports: captureViewports, states: captureStates, entries: capture.entries, failures: capture.failures });
    result.capture_manifest = manifest;
    assert.equal(capture.exitCode, 0, `State captures failed: ${JSON.stringify(capture.failures)}`);
    // Exercise actual dismissal/selection after capture, preserving the captured states.
    await page.goto(`${origin}/?appearance=light`); await page.waitForFunction(() => window.fixtureReady === true);
    await page.locator("#component-menu-opener").focus(); await page.keyboard.press("Enter"); await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "component-menu-opener");
    await page.locator("#component-menu-opener").focus(); await page.keyboard.press("Enter"); await page.keyboard.press("Enter");
    assert.deepEqual(await page.evaluate(() => window.buttonFixture.log), ["First choice"]);
    if (captureStates.includes("confirm")) {
      await page.locator("#confirm-action").focus(); await page.keyboard.press("Enter"); await page.keyboard.press("Escape");
      assert.equal(await page.evaluate(() => document.activeElement?.id), "confirm-action");
      assert.deepEqual(await page.evaluate(() => window.buttonFixture.log), ["First choice", "confirm:null"]);
    }
    assert.deepEqual(browserErrors, [], "No component page errors after dismissal checks");
    result.status = "PASS";
  } catch (error) {
    result.error = error instanceof Error ? error.stack : String(error); process.exitCode = 1;
    if (activePage) { try { await activePage.screenshot({ path: path.join(output, "failure.png"), fullPage: true }); } catch (captureError) { result.failure_screenshot_error = String(captureError); } }
  } finally {
    const cleanupErrors = [];
    if (browser) { try { await browser.close(); } catch (error) { cleanupErrors.push(String(error)); } }
    if (server) { try { server.closeAllConnections(); await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())); } catch (error) { cleanupErrors.push(String(error)); } }
    if (cleanupErrors.length) { result.status = "FAIL"; result.cleanup_errors = cleanupErrors; process.exitCode = 1; }
    result.browser_errors = browserErrors;
    await writeFile(path.join(output, "buttons.json"), JSON.stringify(result, null, 2) + "\n", { mode: 0o600 });
  }
}

await main();

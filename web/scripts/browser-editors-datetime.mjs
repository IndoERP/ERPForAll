/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled shipped components with explicit caller data; no backend/auth/persistence acceptance.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, stat } from "node:fs/promises";
import { hostname, homedir } from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { auditPage } from "../testing/axe.ts";

const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.EDITOR_BROWSER_OUTPUT;
assert.ok(output, "Set EDITOR_BROWSER_OUTPUT to a task-owned evidence directory.");
await mkdir(output, { recursive: true, mode: 0o700 });
const qa = path.join(homedir(), ".local/share/erpforall-qa");
assert.equal((await stat(qa)).mode & 0o077, 0, "The operator-owned browser directory must be private.");
const require = createRequire(import.meta.url);
const { chromium } = require(path.join(qa, "node_modules/playwright"));
const { build } = require(path.join(root, "web/node_modules/esbuild"));
const hash = data => createHash("sha256").update(data).digest("hex");
const client = name => JSON.stringify(path.join(root, "addons/web/static/src/client", name));
const entry = `
import { MessageServices } from ${client("messages.ts")};
import { createDateTimeEditor, createDateTimeRangeEditor } from ${client("datetime-editor.ts")};
import { createTimeEditor } from ${client("time-editor.ts")};
const scene=document.querySelector('#scene');
const locale={lang:'en_GB',tz:'Asia/Jakarta',date_format:'%d/%m/%Y',time_format:'%H:%M:%S'};
let services, handles=[], changes=[], invalid=[], failures=[], siblingChanges=[];
const report=e=>failures.push({kind:e?.kind,message:e instanceof Error?e.message:String(e)});
function reset(){for(const h of handles)h.dispose();handles=[];services?.dispose();scene.replaceChildren();changes=[];invalid=[];failures=[];siblingChanges=[];services=new MessageServices(scene,report);}
function show(state){reset();
 const common={locale,popovers:services.popovers,report,onApply:v=>changes.push(v),onInvalid:e=>invalid.push({kind:e.kind,message:e.message})};
 let h;
 if(state.startsWith('unsupported-')){
   const invalidLocale={...locale,...(state==='unsupported-time'?{time_format:'%f'}:{date_format:'%j'})};
   const configured={...common,locale:invalidLocale,name:'unavailable',label:'Configured editor',value:state==='unsupported-time'?'09:15:30':state==='unsupported-datetime'?'2026-11-10 02:15:30':'2026-11-10'};
   h=state==='unsupported-time'?createTimeEditor(configured):createDateTimeEditor({...configured,kind:state==='unsupported-datetime'?'datetime':'date'});
 }
 else if(state==='time')h=createTimeEditor({...common,name:'time',label:'Start time',value:'09:15:30',showSeconds:true,rounding:0});
 else if(state==='range')h=createDateTimeRangeEditor({...common,kind:'datetime',names:['start','end'],labels:['Start','End'],value:['2026-11-10 02:15:30','2026-11-10 03:15:30'],required:[true,false],showSeconds:true,rounding:0});
 else {const datetime=state==='datetime'||state==='dialog';h=createDateTimeEditor({...common,kind:datetime?'datetime':'date',name:'scheduled',label:datetime?'Scheduled at':'Order date',value:state==='empty'||state==='required'?false:datetime?'2026-11-10 02:15:30':'2026-11-10',min:datetime?undefined:'2026-11-05',max:datetime?undefined:'2026-11-25',readonly:state==='readonly',required:state==='required',showSeconds:true,rounding:0});}
 if(state.startsWith('unsupported-')){const label=document.createElement('label'),input=document.createElement('input');label.htmlFor='unrelated-note';label.textContent='Other note';input.id='unrelated-note';input.name='unrelated-note';input.addEventListener('change',()=>siblingChanges.push(input.value));scene.append(label,input);}
 handles.push(h);if(state==='dialog')services.dialogs.open({title:'Edit appointment',body:h.node});else scene.append(h.node);
}
window.fixture={show,reset,state:()=>({changes,invalid,failures,siblingChanges}),abort:()=>{for(const h of handles)h.dispose();}};
show('date');document.body.dataset.ready='true';
`;
const bundle = await build({ stdin: { contents: entry, resolveDir: root, sourcefile: "datetime-browser-fixture.ts", loader: "ts" }, bundle: true, write: false, minify: true, platform: "browser", format: "esm", target: "es2023", metafile: true, alias: { "@erpforall/elang": path.join(root, "web/src/index.ts"), "@erpforall/expr": path.join(root, "web/packages/expr/src/index.ts") } });
const javascript = bundle.outputFiles[0].contents;
const legalNoticeRetained = ["Copyright 2019 JS Foundation and other contributors", "Permission is hereby granted, free of charge", 'THE SOFTWARE IS PROVIDED "AS IS"'].every(text=>Buffer.from(javascript).toString().includes(text));
assert.ok(legalNoticeRetained, "The compiled bundle must retain the upstream Luxon MIT notice.");
const css = await readFile(path.join(root, "addons/web/static/src/client/client.css"));
const sources = [];
for (const filename of Object.keys(bundle.metafile.inputs).sort()) {
  if (filename === "datetime-browser-fixture.ts" || filename.includes("node_modules")) continue;
  const absolute = path.resolve(root, filename);
  sources.push({ path: path.relative(root, absolute), sha256: hash(await readFile(absolute)) });
}
for (const filename of ["addons/web/static/src/client/client.css", "web/pnpm-lock.yaml", "web/scripts/browser-editors-datetime.mjs", "web/testing/axe.ts"]) sources.push({ path: filename, sha256: hash(await readFile(path.join(root, filename))) });
const server = createServer((request, response) => {
  if (request.url === "/fixture.js") { response.writeHead(200, { "Content-Type": "text/javascript; charset=utf-8" }); response.end(javascript); }
  else if (request.url === "/client.css") { response.writeHead(200, { "Content-Type": "text/css; charset=utf-8" }); response.end(css); }
  else if (request.url === "/") { response.writeHead(200, { "Content-Type": "text/html; charset=utf-8" }); response.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Date and time editor acceptance</title><link rel="stylesheet" href="/client.css"></head><body><main style="padding:24px;max-width:800px"><h1>Date and time editors</h1><section id="scene" class="record-form" aria-label="Editor fixture"></section><button type="button" id="after">Next field</button></main><script type="module" src="/fixture.js"></script></body></html>'); }
  else { response.writeHead(404); response.end(); }
});
await new Promise((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
const origin = `http://127.0.0.1:${server.address().port}`;
const result = { generated_at: new Date().toISOString(), host: hostname(), node: process.version, revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), boundary: "Compiled component fixture; no backend/authentication/persistence acceptance", legal_notice_retained:legalNoticeRetained, bundle_sha256:hash(javascript), sources, checks: [], captures: [], errors: [] };
let browser, page;
try {
  browser = await chromium.launch({ headless: true }); result.browser = browser.version();
  page = await browser.newPage(); page.setDefaultTimeout(8000);
  page.on("pageerror", error => result.errors.push(error.message));
  page.on("console", message => { if (message.type() === "error") result.errors.push(message.text()); });
  await page.goto(origin); await page.waitForSelector('body[data-ready="true"]');
  const show = async state => { await page.evaluate(value => window.fixture.show(value), state); };
  const state = () => page.evaluate(() => window.fixture.state());
  await show("date");
  await page.getByRole("textbox", { name: "Order date", exact: true }).fill("15/11/2026");
  await page.getByRole("textbox", { name: "Order date", exact: true }).press("Enter");
  assert.deepEqual((await state()).changes, ["2026-11-15"]); result.checks.push("typed date applies ISO once");
  await show("date"); await page.getByRole("button", { name: "Choose Order date", exact: true }).click();
  const input = page.getByRole("textbox", { name: "Order date", exact: true });
  await input.fill("not a date"); await input.press("Escape");
  assert.equal(await input.inputValue(), "10/11/2026"); assert.deepEqual((await state()).changes, []);
  result.checks.push("Escape restores without applying");
  await show("datetime"); const datetime = page.getByRole("textbox", { name: "Scheduled at", exact: true });
  await datetime.fill("11/11/2026 10:45:20"); await datetime.press("Enter");
  assert.deepEqual((await state()).changes, ["2026-11-11 03:45:20"]); result.checks.push("configured zone serializes UTC datetime");
  await show("time"); const time = page.locator('[name="time"]');
  await time.fill("invalid"); await time.dispatchEvent("change");
  assert.equal((await state()).invalid.length, 1); assert.deepEqual((await state()).changes, []); result.checks.push("invalid time callback without valid application");
  await show("datetime"); await page.getByRole("button", {name:"Choose Scheduled at",exact:true}).click();
  await page.locator('[name="scheduled_time"]').fill("14:25:40");
  await page.locator('#after').click();
  assert.deepEqual((await state()).changes,["2026-11-10 07:25:40"]);
  assert.equal(await page.locator('.message-popover').count(),0);
  assert.equal(await page.locator('#after').evaluate(node=>node===document.activeElement),true);
  result.checks.push("outside pointer departure applies composed typed time once without stealing focus");
  await show("datetime"); await page.getByRole("button", {name:"Choose Scheduled at",exact:true}).click();
  await page.locator('[name="scheduled"]').fill("12/11/2026 15:35:50");
  await page.locator('#after').click();
  assert.deepEqual((await state()).changes,["2026-11-12 08:35:50"]);
  assert.equal(await page.locator('.message-popover').count(),0);
  result.checks.push("outside pointer departure preserves main datetime typing while calendar is open");
  await show("dialog"); await page.getByRole("button", {name:"Choose Scheduled at",exact:true}).click();
  await page.keyboard.press("Escape");
  assert.equal(await page.locator('dialog.message-dialog[open]').count(),1); assert.equal(await page.locator('.message-popover').count(),0);
  assert.equal(await page.locator('[name="scheduled"]').evaluate(input=>input===document.activeElement),true);
  assert.deepEqual((await state()).changes,[]); result.checks.push("nested Escape closes calendar and returns field focus while retaining modal");
  await page.getByRole("button", {name:"Choose Scheduled at",exact:true}).click();
  await page.getByRole("button", {name:"Close",exact:true}).click();
  await page.waitForSelector('.message-popover',{state:'detached'});
  assert.equal(await page.locator('dialog.message-dialog').count(),0); assert.deepEqual((await state()).changes,[]); assert.deepEqual((await state()).failures,[]);
  result.checks.push("owner native dialog closure releases calendar without apply or page errors");
  for (const theme of ["light", "dark"]) for (const viewport of [{name:"desktop",width:1280,height:900},{name:"middle",width:820,height:900},{name:"phone",width:390,height:844}]) {
    await page.setViewportSize({width:viewport.width,height:viewport.height}); await page.emulateMedia({ reducedMotion: "reduce" });
    await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
    for (const scene of ["date", "datetime", "dialog", "time", "range", "readonly", "empty", "required", "unsupported-date", "unsupported-datetime", "unsupported-time", ...(viewport.width>700?["invalid"]:[])]) {
      result.current_scene={theme,viewport,scene};
      await show(scene === "invalid" ? "time" : scene);
      if (["date", "datetime", "dialog"].includes(scene)) await page.getByRole("button", { name: scene === "date" ? "Choose Order date" : "Choose Scheduled at", exact: true }).click();
      if (scene === "dialog") {
        assert.equal(await page.locator('.message-popover').evaluate(node=>node.matches(':popover-open')), true, "Modal calendar must use the shared service's native top layer.");
        assert.equal(await page.locator('dialog.message-dialog').evaluate(node=>node.scrollTop), 0, "Opening the calendar must not scroll its parent labels out of view.");
        const modal = await page.locator('dialog.message-dialog').boundingBox();
        const label = await page.locator('dialog.message-dialog > .message-dialog-body .datetime-editor > label').boundingBox();
        assert.ok(modal && label && label.x>=modal.x && label.y>=modal.y && label.x+label.width<=modal.x+modal.width+1 && label.y+label.height<=modal.y+modal.height+1, "The parent field label must remain visible inside the modal.");
      }
      if (scene === "time") await page.locator('[name="time"]').focus();
      if (scene === "invalid") { const control=page.locator('[name="time"]'); await control.fill(viewport.width<=700?"":"invalid"); await control.dispatchEvent("change"); }
      if (scene === "range") {
        const endpoints = await page.locator('.datetime-range-editor .datetime-editor').evaluateAll(nodes=>nodes.map(node=>{const box=element=>{const r=element.getBoundingClientRect();return {left:r.left,right:r.right,top:r.top,bottom:r.bottom,width:r.width};};return {label:box(node.querySelector('label')),input:box(node.querySelector('input'))};}));
        assert.equal(endpoints.length,2);
        for(const endpoint of endpoints) { assert.ok(endpoint.input.width>=160,"Range endpoint text must remain usable, rather than collapsing beside inherited form labels."); assert.ok(endpoint.label.bottom<=endpoint.input.top+1||endpoint.label.right<=endpoint.input.left+1,"Endpoint label must not overlap its control."); }
        const [first,second]=endpoints;
        assert.ok(first.input.right<=second.label.left+1||first.input.bottom<=second.label.top+1,"Adjacent range endpoints must not overlap labels and inputs.");
      }
      if(scene === "invalid") {
        const validation=page.locator('.editor-validation:not([hidden])'); assert.equal(await validation.count(),1);
        const error=await validation.boundingBox(); assert.ok(error&&error.width>0&&error.height>0);
        for(const overlay of await page.locator('.message-popover').all()) { const box=await overlay.boundingBox(); if(box) assert.ok(error.x+error.width<=box.x||box.x+box.width<=error.x||error.y+error.height<=box.y||box.y+box.height<=error.y,"Time validation message must not be obscured by suggestions."); }
      }
      if(scene.startsWith("unsupported-")) {
        const directive=scene==="unsupported-time"?"%f":"%j", message=`Unsupported date/time format directive: ${directive}.`;
        const control=page.locator('[name="unavailable"]'), validation=page.locator('.editor-validation:not([hidden])');
        assert.equal(await control.isDisabled(),true); assert.equal(await control.getAttribute('aria-invalid'),'true');
        assert.equal(await validation.count(),1); assert.equal(await validation.textContent(),message);
        const box=await validation.boundingBox(); assert.ok(box&&box.width>0&&box.height>0,"Configuration feedback must remain visible.");
        assert.equal(await page.locator('.message-popover').count(),0);
        const before=await state(); assert.deepEqual(before.invalid,[{kind:'unsupported',message}]); assert.deepEqual(before.changes,[]);
        const sibling=page.getByRole('textbox',{name:'Other note',exact:true}); await sibling.fill('Settings remain editable'); await sibling.press('Tab');
        assert.equal(await page.locator('#after').evaluate(node=>node===document.activeElement),true);
        const after=await state(); assert.deepEqual(after.siblingChanges,['Settings remain editable']); assert.deepEqual(after.changes,[]);
      }
      const labels = await page.locator('input').evaluateAll(inputs=>inputs.map(input=>({id:input.id,labels:[...input.labels].map(label=>label.htmlFor)})));
      for(const input of labels) assert.deepEqual(input.labels,[input.id],"Each editor input needs one native visible label.");
      const geometry = await page.evaluate(() => ({ width: window.innerWidth, scrollWidth: document.documentElement.scrollWidth, overlays: [...document.querySelectorAll('.message-popover')].map(node=>{const r=node.getBoundingClientRect();return {left:r.left,top:r.top,right:r.right,bottom:r.bottom,width:r.width,presentation:node.dataset.presentation};}) }));
      assert.ok(geometry.scrollWidth <= geometry.width + 1, `${theme}/${viewport.name}/${scene}: horizontal overflow`);
      for (const box of geometry.overlays) { assert.ok(box.left>=0 && box.right<=viewport.width+1 && box.top>=0 && box.bottom<=viewport.height+1, `Overlay outside viewport: ${JSON.stringify(box)}`); }
      for (const box of geometry.overlays) if(box.presentation==='anchored'&&['date','datetime','dialog'].includes(scene)) assert.ok(box.width<=360,"Anchored calendars must remain compact and leave the parent controls accessible.");
      let axe;
      try { axe = await auditPage(page); }
      catch (failure) {
        const prefix = `${theme}-${viewport.name}-${scene}-failure`;
        const diagnostics = await page.evaluate(async () => ({ axe: await window.axe.run(document), html: document.documentElement.outerHTML,
          labels: [...document.querySelectorAll('input')].map(input=>({ id:input.id, role:input.getAttribute('role'), labelledby:input.getAttribute('aria-labelledby'), labels:[...(input.getAttribute('aria-labelledby')??'').split(' ')].filter(Boolean).map(id=>{const label=document.getElementById(id);return {id,text:label?.textContent,display:label?getComputedStyle(label).display:null,visibility:label?getComputedStyle(label).visibility:null};}) })) }));
        await writeFile(path.join(output, prefix + ".json"), JSON.stringify(diagnostics, null, 2));
        await page.screenshot({path:path.join(output,prefix+".png"),fullPage:true});
        result.failed_scene={theme,viewport,scene,diagnostics:prefix+".json"}; throw failure;
      }
      assert.deepEqual(axe.violations, [], `axe findings: ${axe.violations.map(v=>v.id).join(", ")}`);
      const name = `${theme}-${viewport.name}-${scene}`;
      const png = await page.screenshot({ fullPage: true }); await writeFile(path.join(output, name + ".png"), png);
      await writeFile(path.join(output, name + "-axe.json"), JSON.stringify(axe, null, 2));
      result.captures.push({ name, viewport, theme, scene, sha256:hash(png), bytes:png.length, geometry, axe_violations:axe.violations.length });
      assert.deepEqual((await state()).failures, []);
    }
  }
  assert.deepEqual(result.errors, []); result.status = "passed";
} catch (error) {
  result.status="failed"; result.failure=error instanceof Error?error.stack:String(error);
  if(page) { try { await page.screenshot({path:path.join(output,"failure.png"),fullPage:true}); } catch(captureError) { result.failure_screenshot_error=String(captureError); } }
  throw error;
}
finally {
  if (browser) await browser.close();
  server.closeAllConnections(); await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  await writeFile(path.join(output, "manifest.json"), JSON.stringify(result, null, 2));
}

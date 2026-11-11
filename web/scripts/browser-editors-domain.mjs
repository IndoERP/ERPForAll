/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled reusable editor acceptance. Caller fixtures do not prove persistence or authorization.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { homedir, hostname } from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { auditPage } from "../testing/axe.ts";
const root = fileURLToPath(new URL("../../", import.meta.url)),
  output = process.env.DOMAIN_BROWSER_OUTPUT;
assert.ok(output, "Set DOMAIN_BROWSER_OUTPUT to a task-owned directory.");
await mkdir(output, { recursive: true, mode: 0o700 });
const qa = path.join(homedir(), ".local/share/erpforall-qa");
assert.equal(
  (await stat(qa)).mode & 0o077,
  0,
  "Browser tooling must be operator-owned and private.",
);
const require = createRequire(import.meta.url),
  { chromium } = require(path.join(qa, "node_modules/playwright")),
  { build } = require(path.join(root, "web/node_modules/esbuild"));
const hash = (data) => createHash("sha256").update(data).digest("hex");
const client = (name) =>
  JSON.stringify(path.join(root, "addons/web/static/src/client", name));
const entry = `
import {MessageServices} from ${client("messages.ts")};
import {createDomainEditor,createExpressionEditor,createTreeEditor} from ${
  client("public.ts")
};
import {defaultTreeEditorProvider} from ${client("tree-editor-fields.ts")};
const host=document.querySelector('#scene'); let messages,editor,changes=[],errors=[],releases=[],pending=false,fail=false,hold=false,ownerWaiters=[];
const field=(name,kind,relation=null)=>({name,label:({name:'Name',amount:'Amount',active:'Active',partner_id:'Contact'})[name]||name,kind,relation,relationField:null,required:false,readonly:false,selection:null});
const base=new Map([['name',field('name','char')],['amount',field('amount','integer')],['active',field('active','boolean')],['partner_id',field('partner_id','many2one','res.partner')]]);
const map=model=>model==='res.partner'?new Map([['name',field('name','char')]]):base;
async function show(state){editor?.dispose();messages?.dispose();host.replaceChildren();changes=[];errors=[];releases=[];pending=state==='loading';fail=false;hold=false;ownerWaiters=[];
 messages=new MessageServices(host,e=>errors.push(e.message));
 const environment={messages,report:e=>errors.push(e.message)};
 const fields=(model,signal)=>pending?new Promise(resolve=>{releases.push(()=>resolve(map(model)));}):state==='metadata-error'?Promise.reject(new Error('Fixture metadata unavailable')):map(model);
 const change=async result=>{if(fail)throw new Error('Fixture caller refused edit');changes.push(result);if(hold)await new Promise(resolve=>ownerWaiters.push(resolve));};
 const common={environment,label:'Record rules',model:'demo.record',fields,change,debug:true,readonly:state==='readonly',touch:innerWidth<=700&&navigator.maxTouchPoints>0};
 if(state.startsWith('expression'))editor=createExpressionEditor({...common,label:'Condition',value:state==='expression-unsupported'?'context.get("secret")':'active and amount > 0',fields:()=>new Map([...base,['properties',{name:'properties',label:'Properties',kind:'properties'}]])});
 else if(state==='tree'||state==='deep'||state==='tree-locked'){let value={id:'a',kind:'condition',path:'name',operator:'=',value:{kind:'string',value:'Rina'}};if(state==='deep'){for(let i=0;i<30;i++)value={id:'deep-'+i,kind:i%2?'and':'or',children:[value]};}else value={id:'root',kind:'and',children:[value]};const provider=state==='tree-locked'?{...defaultTreeEditorProvider('domain'),valueEditor:()=>{const node=document.createElement('input');node.disabled=true;node.dataset.lockedProvider='';node.setAttribute('aria-label','Locked value');return {node,dispose(){}};}}:undefined;editor=createTreeEditor({...(({debug,...rest})=>rest)(common),value,...(provider?{provider}:{})});}
 else editor=createDomainEditor({...common,value:state==='unsupported'?"[('name', 'child_of', 1)]":state==='invalid'?"['|']":state==='empty'?'[]':state==='long'?"[('name', '=', '"+'Long'.repeat(128)+"')]":"[('active', 'in', [True, False]), ('name', '=', 'Rina'), ('amount', '>', 0)]"});
 host.append(editor.node);if(state!=='loading')await editor.ready;return true;
}
window.fixture={show,state:()=>({changes,errors,state:editor.node.dataset.state}),ready:()=>editor.ready,release:()=>{pending=false;for(const release of releases.splice(0))release();},fail:()=>{fail=true;},hold:()=>{hold=true;},settle:()=>{hold=false;for(const done of ownerWaiters.splice(0))done();},dispose:()=>editor.dispose()};
await show('domain');document.body.dataset.ready='true';
`;
const bundle = await build({
  stdin: {
    contents: entry,
    resolveDir: root,
    sourcefile: "domain-browser-fixture.ts",
    loader: "ts",
  },
  bundle: true,
  write: false,
  minify: true,
  platform: "browser",
  format: "esm",
  target: "es2023",
  metafile: true,
  alias: {
    "@erpforall/elang": path.join(root, "web/src/index.ts"),
    "@erpforall/expr": path.join(root, "web/packages/expr/src/index.ts"),
  },
});
const javascript = bundle.outputFiles[0].contents,
  css = await readFile(
    path.join(root, "addons/web/static/src/client/client.css"),
  );
const sources = [];
for (const file of Object.keys(bundle.metafile.inputs).sort()) {
  if (file === "domain-browser-fixture.ts" || file.includes("node_modules")) {
    continue;
  }
  sources.push({
    path: file,
    sha256: hash(await readFile(path.resolve(root, file))),
  });
}
for (
  const file of [
    "addons/web/static/src/client/client.css",
    "web/scripts/browser-editors-domain.mjs",
    "web/pnpm-lock.yaml",
  ]
) {
  sources.push({
    path: file,
    sha256: hash(await readFile(path.join(root, file))),
  });
}
const server = createServer((request, response) => {
  if (request.url === "/fixture.js") {
    response.writeHead(200, {
      "Content-Type": "text/javascript; charset=utf-8",
    });
    response.end(javascript);
  } else if (request.url === "/client.css") {
    response.writeHead(200, { "Content-Type": "text/css; charset=utf-8" });
    response.end(css);
  } else if (request.url === "/") {
    response.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    response.end(
      '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Structured editors</title><link rel="stylesheet" href="/client.css"></head><body><main style="padding:16px;max-width:900px;margin:auto"><h1>Structured editors</h1><section id="scene" aria-label="Editor fixture"></section><button type="button" id="after">Next control</button></main><script type="module" src="/fixture.js"></script></body></html>',
    );
  } else {
    response.writeHead(404);
    response.end();
  }
});
await new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolve);
});
const origin = `http://127.0.0.1:${server.address().port}`;
const result = {
  utc: new Date().toISOString(),
  host: hostname(),
  node: process.version,
  command: process.argv,
  revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(),
  boundary:
    "Compiled reusable components with caller metadata; no installed-view/backend authorization/persistence proof",
  bundle_sha256: hash(javascript),
  sources,
  checks: [],
  captures: [],
  errors: [],
};
const widths = process.env.DOMAIN_BROWSER_WIDTHS
  ? process.env.DOMAIN_BROWSER_WIDTHS.split(",").map(Number)
  : [1280, 820, 390];
const themes = process.env.DOMAIN_BROWSER_THEMES
  ? process.env.DOMAIN_BROWSER_THEMES.split(",")
  : ["light", "dark"];
assert.ok(
  widths.length && widths.every((width) => [1280, 820, 390].includes(width)),
  "Invalid DOMAIN_BROWSER_WIDTHS",
);
assert.ok(
  themes.length && themes.every((theme) => ["light", "dark"].includes(theme)),
  "Invalid DOMAIN_BROWSER_THEMES",
);
let browser, lastPage;
try {
  browser = await chromium.launch({ headless: true });
  result.browser = browser.version();
  for (const theme of themes) {
    for (const width of widths) {
      const context = await browser.newContext({
          viewport: { width, height: 1000 },
          colorScheme: theme,
          hasTouch: width === 390,
          isMobile: width === 390,
          reducedMotion: "reduce",
        }),
        page = await context.newPage();
      // This pinned browser resets native touch emulation after full-page capture.
      // device-probe.mjs records the observed reset; restore it through CDP.
      const device = await context.newCDPSession(page);
      const restoreTouch = async () => {
        if (width === 390) {
          await device.send("Emulation.setTouchEmulationEnabled", {
            enabled: true,
            maxTouchPoints: 1,
          });
        }
      };
      page.setDefaultTimeout(10000);
      lastPage = page;
      page.on("pageerror", (e) => result.errors.push(e.message));
      page.on("console", (m) => {
        if (m.type() === "error") result.errors.push(m.text());
      });
      await page.goto(origin);
      await page.waitForSelector('body[data-ready="true"]');
      await page.evaluate(
        (theme) => document.documentElement.dataset.theme = theme,
        theme,
      );
      const show = (state) =>
        page.evaluate((state) => window.fixture.show(state), state);
      const state = () => page.evaluate(() => window.fixture.state());
      const capture = async (name) => {
        const geometry = await page.evaluate(() => ({
          width: innerWidth,
          scroll: document.documentElement.scrollWidth,
        }));
        assert.ok(
          geometry.scroll <= geometry.width + 1,
          `${name}: no page overflow`,
        );
        const connectors = await page.locator(
          ".editor-tree-group > .editor-tree-controls",
        ).evaluateAll((nodes) =>
          nodes.map((node) => node.getBoundingClientRect().height)
        );
        assert.ok(
          connectors.every((height) => height <= 64),
          `${name}: connector controls stay compact without vertical flex-basis gaps`,
        );
        if (name === "deep") {
          const leaf = await page.locator('[data-tree-node="a"]').boundingBox();
          assert.ok(
            leaf && leaf.width >= Math.min(240, width - 64),
            `Deep authoring retains a usable row width: ${leaf?.width}`,
          );
        }
        const axe = await auditPage(page);
        assert.equal(axe.violations.length, 0, `${name}: zero axe violations`);
        const basename = `${theme}-${width}-${name}`,
          png = await page.screenshot({ fullPage: true });
        await restoreTouch();
        await writeFile(path.join(output, basename + ".png"), png);
        await writeFile(
          path.join(output, basename + ".axe.json"),
          JSON.stringify(axe, null, 2),
        );
        result.captures.push({
          name,
          theme,
          width,
          path: basename + ".png",
          sha256: hash(png),
          axe: basename + ".axe.json",
          violations: axe.violations.length,
        });
      };
      for (
        const name of [
          "domain",
          "empty",
          "unsupported",
          "invalid",
          "readonly",
          "expression",
          "expression-unsupported",
          "tree",
          "deep",
          "long",
          "loading",
          "metadata-error",
        ]
      ) {
        await show(name);
        await capture(name);
        if (name === "loading") {
          await page.evaluate(() => window.fixture.release());
          await page.evaluate(() => window.fixture.ready());
        }
      }
      await show("domain");
      const archive = page.locator("[data-domain-archived]");
      assert.equal(await archive.isChecked(), true);
      await archive.uncheck();
      await page.waitForFunction(() =>
        window.fixture.state().changes.length === 1
      );
      assert.ok(!(await state()).changes[0].source.includes("'in'"));
      result.checks.push({
        theme,
        width,
        check: "archive toggle preserves ordinary conditions",
      });
      const source = page.locator("[data-editor-source]");
      await source.fill("[('name', '=', 'Typed')]");
      await page.waitForFunction(() =>
        window.fixture.state().changes.some((change) =>
          change.source.includes("Typed")
        )
      );
      await capture("dirty");
      await show("unsupported");
      const reset = page.locator("[data-editor-reset]");
      await reset.focus();
      await page.keyboard.press("Enter");
      await page.waitForFunction(() =>
        window.fixture.state().changes.length === 1
      );
      assert.equal((await state()).changes[0].source, "[]");
      result.checks.push({ theme, width, check: "explicit reset by keyboard" });
      await show("tree");
      await page.locator('[data-tree-node="a"] [data-tree-action="nest"]')
        .focus();
      await page.keyboard.press("Enter");
      await page.waitForFunction(() =>
        window.fixture.state().changes.length === 1
      );
      await capture("nested");
      await page.locator('[data-tree-node="a"] [data-field-open]').click();
      await page.locator('[data-field-name="amount"]').waitFor();
      const pickerMode = await page.evaluate(() => ({
        coarse: matchMedia("(pointer:coarse)").matches,
        touchPoints: navigator.maxTouchPoints,
        width: innerWidth,
        sheets: document.querySelectorAll("dialog.editor-sheet").length,
        popovers: document.querySelectorAll(".editor-popover").length,
      }));
      result.checks.push({
        theme,
        width,
        check: "field picker mode",
        ...pickerMode,
      });
      assert.equal(
        pickerMode.touchPoints > 0,
        width === 390,
        "Touch capability matches the requested device",
      );
      assert.equal(
        pickerMode.sheets,
        width === 390 ? 1 : 0,
        "Phone field choices use the shared sheet",
      );

      await capture("field-picker");
      await page.keyboard.press("Escape");
      assert.equal(
        await page.locator('[data-tree-node="a"] [data-field-open]').evaluate(
          (node) => node === document.activeElement,
        ),
        true,
      );
      result.checks.push({
        theme,
        width,
        check: "field picker Escape restores focus",
      });
      await show("domain");
      const native = page.locator("[data-editor-source]"),
        original = await native.inputValue();
      await native.focus();
      await native.press("ControlOrMeta+A");
      await context.grantPermissions(["clipboard-read", "clipboard-write"]);
      await page.evaluate(() => navigator.clipboard.writeText("not_a_domain"));
      await native.press("ControlOrMeta+V");
      await page.waitForFunction(() =>
        window.fixture.state().changes.length === 1 &&
        document.querySelector(".editor-domain").getAttribute("aria-busy") ===
          "false"
      );
      assert.equal(await native.inputValue(), "not_a_domain");
      assert.equal(
        await native.evaluate((node) => node === document.activeElement),
        true,
      );
      await native.press("ControlOrMeta+Z");
      await page.waitForFunction(() =>
        window.fixture.state().changes.length === 2 &&
        document.querySelector(".editor-domain").getAttribute("aria-busy") ===
          "false"
      );
      assert.equal(await native.inputValue(), original);
      result.checks.push({
        theme,
        width,
        check:
          "native paste and undo retain textarea focus and authored source",
      });
      await show("domain");
      await page.evaluate(() => window.fixture.fail());
      const errorSource = page.locator("[data-editor-source]");
      await errorSource.fill("[]");
      await page.waitForFunction(() =>
        window.fixture.state().errors.includes("Fixture caller refused edit")
      );
      await capture("callback-error");
      assert.deepEqual((await state()).errors, ["Fixture caller refused edit"]);
      result.checks.push({theme, width, check: "raw callback rejection reports exactly once"});
      for (const mode of ["domain", "expression"]) {
        await show(mode);
        await page.evaluate(() => window.fixture.hold());
        const draft = page.locator("[data-editor-source]");
        const inputs = mode === "domain"
          ? ["[('name', '=', 'A')]", "[('name', '=', 'B')]", "[('name', '=', 'C')]"]
          : ["amount > 1", "amount > 2", "amount > 3"];
        await draft.fill(inputs[0]);
        await page.waitForFunction(() => window.fixture.state().changes.length === 1);
        assert.equal(await draft.isEditable(), true);
        await draft.fill(inputs[1]);
        await context.grantPermissions(["clipboard-read", "clipboard-write"]);
        await draft.focus();
        await draft.press("ControlOrMeta+A");
        await page.evaluate(text => navigator.clipboard.writeText(text), inputs[2]);
        await draft.press("ControlOrMeta+V");
        assert.equal(await draft.inputValue(), inputs[2]);
        assert.equal((await state()).changes.length, 1);
        await capture(mode + "-pending-raw");
        await page.evaluate(() => window.fixture.settle());
        await page.waitForFunction(() => window.fixture.state().changes.length === 2 &&
          document.querySelector('[data-editor-source]').closest('[role=group]').getAttribute('aria-busy') === 'false');
        assert.deepEqual((await state()).changes.map(change => change.source), [inputs[0], inputs[2]]);
        assert.equal(await draft.inputValue(), inputs[2]);
        assert.deepEqual((await state()).errors, []);
        result.checks.push({theme, width, check: mode + " pending typing/paste retains latest draft and coalesces callbacks"});
      }
      for (const mode of ["domain", "expression"]) {
        await show(mode);
        await page.evaluate(() => window.fixture.fail());
        const operator = page.locator('[data-tree-operator]').first();
        await operator.selectOption('!=');
        await page.waitForFunction(() => window.fixture.state().errors.length > 0);
        assert.deepEqual((await state()).errors, ["Fixture caller refused edit"]);
        assert.equal(await page.locator('.editor-' + mode + ' [role=alert]').count(), 1);
        result.checks.push({theme, width, check: mode + " tree rejection has one report and owning alert"});
      }
      await show("tree-locked");
      assert.equal(await page.locator('[data-locked-provider]').isDisabled(), true);
      await page.locator('[data-tree-node="root"] > .editor-tree-controls [data-tree-connector]').selectOption('or');
      await page.waitForFunction(() => window.fixture.state().changes.length === 1 &&
        document.querySelector('.editor-tree').getAttribute('aria-busy') === 'false');
      assert.equal(await page.locator('[data-locked-provider]').isDisabled(), true);
      result.checks.push({theme, width, check: "provider disabled control stays disabled after another edit"});
      await context.close();
    }
  }
  assert.deepEqual(result.errors, []);
} catch (error) {
  result.failure = error instanceof Error ? error.stack : String(error);
  if (lastPage) {
    result.diagnostic = await lastPage.evaluate(() => ({
      state: window.fixture?.state(),
      source: document.querySelector("[data-editor-source]")?.value,
      readonly: document.querySelector("[data-editor-source]")?.readOnly,
      active: document.activeElement?.outerHTML,
    }));
    await lastPage.screenshot({
      path: path.join(output, "failure.png"),
      fullPage: true,
    });
  }
  throw error;
} finally {
  if (browser) await browser.close();
  await new Promise((resolve) => server.close(resolve));
  result.finished = new Date().toISOString();
  await writeFile(
    path.join(output, "manifest.json"),
    JSON.stringify(result, null, 2) + "\n",
  );
  console.log(JSON.stringify(result));
}

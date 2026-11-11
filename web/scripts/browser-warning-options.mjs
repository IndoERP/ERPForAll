/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real Chromium acceptance using captured output of our native onchange integration test.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { readFile, writeFile, mkdir, lstat, realpath } from 'node:fs/promises';
import { hostname, homedir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../../', import.meta.url));
const capture = process.env.WARNINGS_CAPTURE_DIR, output = process.env.WARNINGS_BROWSER_OUTPUT;
assert.ok(capture && output, 'Set WARNINGS_CAPTURE_DIR and WARNINGS_BROWSER_OUTPUT.');
await mkdir(output);
const qaDir=path.join(homedir(),'.local/share/erpforall-qa'),qaInfo=await lstat(qaDir);
assert.ok(qaInfo.isDirectory() && (qaInfo.mode & 0o777)===0o700 && qaInfo.uid===process.getuid(),'Playwright QA directory must be operator-owned and mode 700.');
const playwrightPath=await realpath(path.join(qaDir,'node_modules/playwright/index.mjs'));
assert.ok(playwrightPath.startsWith((await realpath(qaDir))+path.sep),'Playwright must resolve inside the private QA directory.');
const {chromium}=await import(pathToFileURL(playwrightPath).href);
const playwrightVersion=JSON.parse(await readFile(path.join(path.dirname(playwrightPath),'package.json'),'utf8')).version;
assert.equal(typeof playwrightVersion,'string','Playwright package must declare its version.');
const { build } = await import(pathToFileURL(path.join(root, 'web/node_modules/esbuild/lib/main.js')).href);
const client = name => JSON.stringify(path.join(root, 'addons/web/static/src/client', name));
const captures = {};
const captureReceipt = await readFile(path.join(capture,'..','run.log'),'utf8');
const captureRevision = /^revision: (.+)$/m.exec(captureReceipt)?.[1];
assert.ok(captureRevision, 'Native captures require their run receipt.');
for (const mode of ['sticky', 'timed']) captures[mode] = JSON.parse(await readFile(path.join(capture, `single-${mode}.json`), 'utf8'));
const bundle = await build({ stdin: { contents: `
import { RecordState } from ${client('record.ts')};
import { RpcClient } from ${client('transport.ts')};
import { MessageServices } from ${client('messages.ts')};
const scene = document.querySelector('main');
let service, state;
window.showWarning = async (actual) => {
  service?.dispose(); scene.replaceChildren();
  service = new MessageServices(scene, error => { throw error; });
  const rpc = new RpcClient(async endpoint => new Response(JSON.stringify(String(endpoint).endsWith('onchange') ? actual : [{id:7,name:'Before',note:false}]), {headers:{'Content-Type':'application/json'}}), () => {});
  const fields = ['name','note'].map(name => ({name,kind:'char',label:name,readonly:false,required:false,selection:null,relation:null,relationField:null}));
  state = new RecordState(rpc,'test_web.discussion',fields,{});
  await state.load(7); await state.onchange('name'); service.warning(state.warningDetails);
  return {values:state.values,warning:state.warningDetails,dirty:state.dirty};
};
`, resolveDir: root, sourcefile: 'warnings-browser-fixture.ts', loader: 'ts' }, bundle: true, write: false, format: 'esm', platform: 'browser', target: 'es2023', metafile: true, alias: { '@erpforall/elang': path.join(root, 'web/src/index.ts'), '@erpforall/expr': path.join(root, 'web/packages/expr/src/index.ts') } });
const javascript = bundle.outputFiles[0].contents, css = await readFile(path.join(root,'addons/web/static/src/client/client.css'));
const server = createServer((request, response) => {
  if (request.url === '/fixture.js') { response.setHeader('Content-Type','text/javascript'); response.end(javascript); }
  else if (request.url === '/client.css') { response.setHeader('Content-Type','text/css'); response.end(css); }
  else { response.setHeader('Content-Type','text/html'); response.end('<!doctype html><html lang="en"><meta charset="utf-8"><title>Onchange warning acceptance</title><link rel="stylesheet" href="/client.css"><body><main aria-label="Warning fixture"></main><script type="module" src="/fixture.js"></script></body></html>'); }
});
await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
let browser;
const entries = [], errors = [];
const revision = (await readFile(path.join(root,'REVISION'),'utf8')).trim();
try {
  browser = await chromium.launch({headless:true});
  const page = await browser.newPage();
  page.on('pageerror', error => errors.push(error.message));
  await page.clock.install();
  for (const theme of ['light','dark']) for (const width of [1440,800,390]) for (const mode of ['sticky','timed']) {
    await page.setViewportSize({width,height:900});
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(() => typeof window.showWarning === 'function');
    await page.evaluate(theme => {document.documentElement.dataset.theme=theme;},theme);
    const actual = await page.evaluate(value => window.showWarning(value),captures[mode]);
    assert.equal(actual.values.note,'noted'); assert.equal(actual.dirty,true);
    assert.deepEqual(actual.warning,captures[mode].warning);
    const notice = page.locator('.client-notification');
    assert.equal(await notice.count(),1); assert.equal(await notice.getAttribute('data-level'),'warning');
    assert.equal(await notice.evaluate(node => node.classList.contains('addon_notice') && node.classList.contains('_urgent-2')),true);
    const box = await notice.boundingBox(); assert.ok(box && box.width > 0 && box.height > 0,'Notification must have real geometry');
    const file = `${theme}-${width}-${mode}.png`; await page.screenshot({path:path.join(output,file),fullPage:true});
    await page.clock.runFor(4001);
    assert.equal(await notice.count(),mode === 'sticky' ? 1 : 0);
    if(mode === 'sticky') {
      const close = page.getByRole('button',{name:'Close notification',exact:true}); await close.focus(); await page.keyboard.press('Enter'); assert.equal(await notice.count(),0);
    }
    entries.push({theme,width,mode,screenshot:file,values:actual.values});
  }
  assert.deepEqual(errors,[]);
  const sha256 = data => createHash('sha256').update(data).digest('hex');
  const sources=[];
  for(const source of Object.keys(bundle.metafile.inputs).sort()) if(source !== 'warnings-browser-fixture.ts') sources.push({path:source,sha256:sha256(await readFile(path.resolve(root,source)))});
  await writeFile(path.join(output,'result.json'),JSON.stringify({status:'passed',host:hostname(),time:new Date().toISOString(),revision,node:process.version,browser:browser.version(),playwright:{directory:qaDir,version:playwrightVersion,mode:(qaInfo.mode & 0o777).toString(8),uid:qaInfo.uid},capture_revision:captureRevision,captureHashes:Object.fromEntries(Object.entries(captures).map(([key,value])=>[key,sha256(JSON.stringify(value))])),entries,errors,sources},null,2)+'\n');
} finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }

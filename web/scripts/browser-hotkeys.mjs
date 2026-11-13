/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Native keyboard proof over real controllers with explicit protocol/template fixtures.
// No authenticated server, compiled XML, persistence or whole-view parity claim.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { hostname, homedir } from 'node:os';
import { createRequire } from 'node:module';
import path from 'node:path';
import { readFile, writeFile, mkdir, stat } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { build } from 'esbuild';

import { auditPage } from '../testing/axe.ts';
const root = new URL('../../', import.meta.url).pathname;
const qa = path.join(homedir(), '.local/share/erpforall-qa');
assert.equal((await stat(qa)).mode & 0o077, 0, 'Operator QA directory must be private (mode 700)');
const qaRequire = createRequire(path.join(qa, 'package.json'));
const { chromium } = qaRequire('playwright');
const output = process.env.HOTKEYS_BROWSER_OUTPUT;
assert.ok(output, 'Set HOTKEYS_BROWSER_OUTPUT to a task-owned evidence directory');
await mkdir(output, { recursive: true });
const client = name => JSON.stringify(root + 'addons/web/static/src/client/' + name);
const entry = `
import { createHotkeyController } from ${JSON.stringify(root + 'web/src/index.ts')};
import { HumanApp } from ${client('app.ts')};
import { configureClientTemplates } from ${client('registry.ts')};
import { MessageServices } from ${client('messages.ts')};
const host=document.querySelector('#app'), mods={readonly:null,required:null,invisible:null};
configureClientTemplates([{name:'web.HumanControlPanel',render:()=>{const node=document.createElement('section');node.dataset.controlPanel='';for(const key of ['breadcrumbs','search','actions','extension','tools']){const part=document.createElement('div');part.setAttribute('data-control-'+key,'');node.append(part);}return node;}}]);
const field={kind:'field',force_save:false,field:{model:'test.note',name:'name'},widget:'char',options:{},modifiers:mods,label:'Name'};
const description={wire_major:1,registry_version:'r1',description_version:'d1',model:'test.note',fields:{'test.note':{name:{type:'char',label:'Name',required:true,readonly:false,relation:null,relation_field:null,selection:null}}},notices:[],views:[{identity:{kind:'stored',id:1},type:'form',schema_major:1,renderer:'web.form',root:0,nodes:[{kind:'form',record_mode:'record',children:[1]},field]}]};
let savedName='Original', saves=[], actionCount=0, pendingResolve;
const action={wire_major:1,id:20,name:'Notes',model:'test.note',target:'current',views:[{id:1,type:'form'}],domain:null,context:null};
window.fetch=async(path,init={})=>{const body=JSON.parse(String(init.body??'{}'));let value;
if(path.endsWith('erp_load_web_menus'))value={wire_major:1,registry_version:'r1',roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:'Notes',action_id:20,children:[],icon:null}]};
else if(path.endsWith('erp_load_web_action_views'))value={action,views:description};
else if(path.endsWith('erp_load_web_action'))value=action;
else if(path.endsWith('get_views'))value=description;
else if(path.endsWith('web_read'))value=[{id:5,name:savedName}];
else if(path.endsWith('onchange'))value={value:{}};
else if(path.endsWith('web_save')){saves.push(body.vals);if(pendingResolve===true)await new Promise(resolve=>pendingResolve=resolve);savedName=body.vals.name??savedName;value=[{id:5,name:savedName}];}
else value=[];
return new Response(JSON.stringify(value),{headers:{'Content-Type':'application/json'}});};
const bootstrap={wire_major:1,registry_version:'r1',asset_version:'a1',identity:{uid:2,name:'Operator',internal:true},companies:[{id:1,name:'Company'}],current_company_id:1,allowed_company_ids:[1],active_company_ids:[1],locale:{lang:'en_US',tz:'UTC',date_format:'%m/%d/%Y',time_format:'%H:%M:%S'},csrf:{logout:'fixture'}};
const app=new HumanApp(host,bootstrap);await app.start();
const messages=new MessageServices(document.querySelector('#overlays'),error=>{throw error;});
const command=document.querySelector('#fixture-action');command.addEventListener('click',()=>actionCount++);
const keys=createHotkeyController(document.body,{bindings:()=>[{kind:'dom',chord:'alt+x',target:command}],report:error=>{throw error;}});
window.fixture={snapshot:()=>({saves,actionCount}),hold:()=>{pendingResolve=true;},release:()=>{const resolve=pendingResolve;pendingResolve=undefined;if(typeof resolve==='function')resolve();},dialog:()=>{const body=document.createElement('button');body.textContent='Open picker';const modal=messages.dialogs.open({title:'Keyboard dialog',body});body.onclick=()=>{const content=document.createElement('button');content.textContent='Picker choice';messages.popovers.open(body,content,{restoreFocus:true});content.focus();};return modal;},nativeModals:()=>{const parent=document.createElement('dialog'),child=document.createElement('dialog');const a=document.createElement('button'),b=document.createElement('button');a.textContent='Parent modal command';b.textContent='Child modal command';parent.append(a);child.append(b);document.body.append(child,parent);parent.showModal();child.showModal();const runs=[];const controller=createHotkeyController(document.body,{bindings:()=>[{kind:'callback',chord:'alt+x',target:a,run:()=>runs.push('parent')},{kind:'callback',chord:'alt+x',target:b,run:()=>runs.push('child')}],report:error=>{throw error;}});window.fixture.native={parent,child,runs,controller};},dispose:()=>{keys.dispose();messages.dispose();app.dispose();}};
`;
const bundle = await build({stdin:{contents:entry,loader:'ts',resolveDir:root+'web'},bundle:true,write:false,format:'esm',target:'es2023',alias:{'@erpforall/elang':root+'web/src/index.ts','@erpforall/expr':root+'web/packages/expr/src/index.ts'},legalComments:'inline'});
const js=bundle.outputFiles[0].contents;
const html='<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Hotkey controller fixture</title></head><body><div id="app"><div data-client-header></div><div data-client-navigation></div><div data-client-notice></div><div data-client-content></div></div><section aria-label="Keyboard fixture"><button id="fixture-action">Fixture action</button></section><div id="overlays"></div><script type="module" src="/fixture.js"></script></body></html>';
const server=createServer((request,response)=>{response.writeHead(200,{'Content-Type':request.url==='/fixture.js'?'text/javascript':'text/html'});response.end(request.url==='/fixture.js'?js:html);});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
let browser;
try {
browser=await chromium.launch({executablePath:process.env.CHROME_BIN??'/usr/bin/google-chrome',headless:true});
const stamp={host:hostname(),utc:new Date().toISOString(),revision:(await readFile(root+'REVISION','utf8')).trim(),node:process.version,platform:process.platform,ime_proof:'injected composition events plus native shortcut suppression',option_proof:'injected Option-produced character plus native Alt access-key sequence',browser:browser.version(),command:'node web/scripts/browser-hotkeys.mjs',bundle_sha256:createHash('sha256').update(js).digest('hex'),scope:'controller/protocol fixtures; no authenticated server acceptance'};
console.log(JSON.stringify(stamp));
for(const viewport of [{width:1440,height:1000},{width:390,height:844}]){
 const page=await browser.newPage({viewport});const errors=[];page.on('pageerror',error=>errors.push(String(error)));
 try{
 await page.goto(`http://127.0.0.1:${server.address().port}/erp/action-20?id=5&view_type=form`);
 await page.waitForFunction(()=>Boolean(window.fixture));
 const input=page.locator('input[name="name"]');await input.fill('Keyboard saved');
 await input.focus();await page.keyboard.press('Alt+s');await page.waitForFunction(()=>window.fixture.snapshot().saves.length===1&&document.querySelector('.status-badge')?.textContent==='Saved');
 assert.deepEqual(await page.evaluate(()=>window.fixture.snapshot().saves),[{name:'Keyboard saved'}]);
 await input.fill('Discard this');await input.press('Tab');await page.waitForFunction(()=>!document.querySelector('input[name="name"]').disabled);await input.focus();await page.keyboard.press('Alt+j');await page.waitForFunction(()=>document.querySelector('input[name="name"]').value==='Keyboard saved');
 await page.locator('#fixture-action').focus();await page.keyboard.down('Alt');assert.ok(await page.locator('[data-hotkey-hint]').count()>0);await page.keyboard.up('Alt');assert.equal(await page.locator('[data-hotkey-hint]').count(),0);
 await page.keyboard.down('Alt');await page.keyboard.down('x');await page.keyboard.down('x');await page.keyboard.up('x');await page.keyboard.up('Alt');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,1);
 await page.evaluate(()=>document.querySelector('#fixture-action').dispatchEvent(new KeyboardEvent('keydown',{key:'≈',code:'KeyX',altKey:true,bubbles:true,cancelable:true})));assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,2);
 await input.fill('Composing');await input.press('Tab');await page.waitForFunction(()=>!document.querySelector('input[name="name"]').disabled);await input.focus();
 await page.evaluate(()=>document.querySelector('input[name="name"]').dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true})));await page.keyboard.press('Alt+s');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).saves.length,1);
 await page.evaluate(()=>document.querySelector('input[name="name"]').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true})));await page.keyboard.press('Alt+j');await page.waitForFunction(()=>document.querySelector('input[name="name"]').value==='Keyboard saved');
 await page.evaluate(()=>window.fixture.nativeModals());await page.keyboard.press('Alt+x');assert.deepEqual(await page.evaluate(()=>window.fixture.native.runs),['child']);
 for(const attribute of ['aria-hidden','aria-disabled','hidden','style']) {
   await page.evaluate(attribute=>window.fixture.native.child.setAttribute(attribute,attribute==='style'?'display:none':'true'),attribute);await page.keyboard.press('Alt+x');assert.deepEqual(await page.evaluate(()=>window.fixture.native.runs),['child']);assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,2);
   await page.evaluate(attribute=>{window.fixture.native.child.removeAttribute(attribute);window.fixture.native.child.querySelector('button').focus();},attribute);
 }
 await page.evaluate(()=>window.fixture.native.child.close());await page.keyboard.press('Alt+x');assert.deepEqual(await page.evaluate(()=>window.fixture.native.runs),['child','parent']);
 await page.evaluate(()=>window.fixture.native.child.showModal());await page.keyboard.press('Alt+x');assert.deepEqual(await page.evaluate(()=>window.fixture.native.runs),['child','parent','child']);
 await page.evaluate(()=>{const n=window.fixture.native;n.child.close();n.parent.close();n.controller.dispose();n.child.show();});await page.keyboard.press('Alt+x');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,3);
 await page.evaluate(()=>{window.fixture.native.child.close();window.fixture.native.child.remove();window.fixture.native.parent.remove();});
 await page.evaluate(()=>window.fixture.dialog());await page.getByRole('button',{name:'Open picker',exact:true}).click();await page.keyboard.press('Alt+x');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,3);
 await page.keyboard.press('Escape');assert.equal(await page.locator('.message-popover').count(),0);assert.equal(await page.locator('dialog[open]').count(),1);assert.equal(await page.evaluate(()=>document.activeElement?.textContent),'Open picker');
 await page.keyboard.press('Escape');await page.waitForFunction(()=>!document.querySelector('dialog[open]'));
 await input.fill('Pending save');await input.press('Tab');await page.waitForFunction(()=>!document.querySelector('input[name="name"]').disabled);await page.evaluate(()=>window.fixture.hold());await input.focus();await page.keyboard.press('Alt+s');await page.waitForFunction(()=>window.fixture.snapshot().saves.length===2);await page.keyboard.press('Alt+s');await page.keyboard.press('Alt+j');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).saves.length,2);await page.evaluate(()=>window.fixture.release());await page.waitForFunction(()=>document.querySelector('.status-badge')?.textContent==='Saved');
 await auditPage(page);assert.deepEqual(errors,[]);await page.screenshot({path:output+`/keyboard-${viewport.width}.png`,fullPage:true});
 await page.evaluate(()=>window.fixture.dispose());await page.keyboard.press('Alt+x');assert.equal((await page.evaluate(()=>window.fixture.snapshot())).actionCount,3);
 console.log(`PASS native keyboard fixture viewport=${viewport.width}: save/discard, hints, repeat, IME session suppression, Option-produced browser event, native modal reopen order and unavailable-modal exclusion, nested Escape, pending guards, disposal and axe`);
 }finally{await page.close();}
}
await writeFile(output+'/receipt.json',JSON.stringify({...stamp,result:'PASS'},null,2)+'\n');
}finally{await browser?.close();await new Promise((resolve,reject)=>server.close(error=>error?reject(error):resolve()));}

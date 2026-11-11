/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Isolated compiled-source browser fixture. No backend/authentication/persistence acceptance claim.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import { hostname, homedir } from "node:os";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import path from "node:path";

const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.MESSAGES_BROWSER_OUTPUT;
assert.ok(output, "Set MESSAGES_BROWSER_OUTPUT to the task-owned evidence directory.");
await mkdir(output, { recursive: true, mode: 0o700 });
const require = createRequire(import.meta.url);
const { chromium } = require(path.join(homedir(), ".local/share/erpforall-qa/node_modules/playwright"));
const { build } = await import(pathToFileURL(path.join(root, "web/node_modules/esbuild/lib/main.js")).href);
const { auditPage } = await import("../testing/axe.ts");
const client = relative => JSON.stringify(path.join(root, "addons/web/static/src/client", relative));
const hash = data => createHash("sha256").update(data).digest("hex");
const write = async (name, data) => { const target = path.join(output, name); await writeFile(target, data); await chmod(target, 0o600); };
const entry = `
import { MessageServices } from ${client("messages.ts")};
import { button, el } from ${client("dom.ts")};
import { renderRibbon, renderNotificationPermissionAlert, noContentHelp } from ${client("message-widgets.ts")};
import { selectCreateDialog } from ${client("select-create-dialog.ts")};
import { exportDialog } from ${client("export-dialog.ts")};
import { RpcClient } from ${client("transport.ts")};
const scene = document.querySelector('#scene');
let services, resolvePending, releases = [], errors = [], actions = 0, selection, downloaded;
const report = error => { errors.push(error instanceof Error ? error.message : String(error)); };
function reset() { services?.dispose(); scene.replaceChildren(); errors=[]; actions=0; selection=undefined; downloaded=undefined; resolvePending=undefined; releases=[]; services=new MessageServices(scene,report); }
const pending = () => new Promise(resolve => { resolvePending=resolve; });
const plain = size => { const opener=button('Open dialog',()=>{});scene.append(opener);opener.focus();const body=el('div'); for(let i=0;i<14;i++)body.append(el('p','A readable paragraph explains the operation and next steps. Long content remains inside the scrollable dialog body.')); const footer=el('div',undefined,'actions'); const handle=services.dialogs.open({title:'Dialog size '+size,body,size,footer}); footer.append(button('Close dialog',()=>handle.close())); return handle; };
function show(state) {
 reset();
 if(state.startsWith('severity-')) {const level=state.slice(9);services.notifications.add('The operation has returned information you can review.',{title:level[0].toUpperCase()+level.slice(1),level,sticky:true});}
 else if(state==='toast-action')services.notifications.add('Review the completed operation.',{title:'Ready',level:'success',sticky:true,actions:[{label:'Review result',run:()=>{actions++;}}]});
 else if(state==='toast-timed') {services.notifications.add('Timed notice',{delay:250});services.notifications.add('Sticky notice',{sticky:true});scene.querySelector('.client-notification').dispatchEvent(new MouseEvent('mouseenter'));}
 else if(state==='minimal-dialog')services.dialogs.open({title:'Minimal notice',body:'A body-only dialog without optional header or footer.',header:false,size:'sm'});
 else if(state.startsWith('dialog-'))plain(state.slice(7));
 else if(state==='confirm-pending') {plain('lg');services.dialogs.confirm({title:'Confirm operation',message:'Review this choice before continuing.',confirmLabel:'Continue',confirm:pending});}
 else if(state==='picker')selectCreateDialog({dialogs:services.dialogs,rpc:new RpcClient(window.fetch.bind(window),()=>{throw new Error('Fixture session expired');}),model:'test.partner',domain:[['active','=',true]],context:{lang:'en_US',fixture:'messages'},select:value=>{selection=value;},createEdit:async()=>({id:9,display_name:'Saved fixture record'}),report});
 else if(state.startsWith('export-'))exportDialog({dialogs:services.dialogs,fields:[{name:'name',label:'Name'},{name:'reference',label:'Reference'}],formats:[{key:'csv',label:'CSV'}],download:async(fields,format)=>{downloaded={fields,format};await pending();},notice:(message,options)=>services.notifications.add(message,options),report});
 else if(state==='popover'||state==='tooltip') {const anchor=button('More information',()=>{});scene.append(anchor);if(state==='popover')services.popovers.open(anchor,el('p','Additional details are anchored to the selected control.'),{placement:'bottom',role:'region'});else services.tooltips.attach(anchor,{text:'Detailed help for this operation',delay:0});}
 else if(state==='ribbon'||state==='permission-dialog') {scene.append(renderRibbon({text:'A deliberately long approval status label',tooltip:'This ribbon is display content.',backgroundClass:'bg-warning'}));scene.append(renderNotificationPermissionAlert({permission:'denied',open:()=>services.dialogs.alert({title:'Browser notification permission',message:'Notifications are blocked. Review this site in your browser settings to change the permission.'})}));}
 else if(state==='empty')scene.append(noContentHelp('No records yet','Create a record to start working here.'));
 else if(state.startsWith('network-'))services.network.set(state.slice(8));
 else if(state==='block-delayed')releases.push(services.block.begin({delay:60000}));
 else if(state==='block-nested'){releases.push(services.block.begin({delay:0}),services.block.begin({delay:0,message:'Preparing the next step…'}));}
 else if(state==='effect')services.effects.show('default',{message:'Operation completed',enabled:true});
 else throw new Error('Unknown fixture state: '+state);
}
window.fixture={show,reset,resolve:()=>resolvePending?.(),release:index=>releases[index]?.(),status:()=>({errors,actions,selection,downloaded,blocked:services.block.blocked}),closeAll:()=>services.dialogs.dispose()};
reset();document.body.dataset.ready='true';
`;
const bundled = await build({ stdin: { contents: entry, resolveDir: root, sourcefile: "message-browser-fixture.ts", loader: "ts" }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023", metafile: true, alias: { "@erpforall/elang": path.join(root,"web/src/index.ts"), "@erpforall/expr": path.join(root,"web/packages/expr/src/index.ts") } });
const javascript = bundled.outputFiles[0].contents;
const css = await readFile(path.join(root,"addons/web/static/src/client/client.css"));
await write("fixture.js",javascript);await write("client.css",css);
const sourceFiles=[];
for(const filename of Object.keys(bundled.metafile.inputs).sort()) {if(filename==='message-browser-fixture.ts')continue;const absolute=path.resolve(root,filename);sourceFiles.push({path:path.relative(root,absolute),sha256:hash(await readFile(absolute))});}
for(const filename of ['web/scripts/browser-messages.mjs','web/testing/axe.ts','web/pnpm-lock.yaml'])sourceFiles.push({path:filename,sha256:hash(await readFile(path.join(root,filename)))});
const rpcCalls=[];
const server=createServer(async(request,response)=>{
 try {
  if(request.method==='POST'&&request.url==='/erp/json/2/test.partner/web_name_search') {
   let body='';for await(const chunk of request){body+=chunk;if(body.length>16384)throw new Error('Fixture request too large');}
   const args=JSON.parse(body);assert.deepEqual(args.domain,[['active','=',true]]);assert.deepEqual(args.context,{lang:'en_US',fixture:'messages'});rpcCalls.push(args);
   response.writeHead(200,{'Content-Type':'application/json'});response.end(JSON.stringify([{id:4,display_name:'Fixture partner'},{id:7,display_name:'Second fixture partner'}]));return;
  }
  if(request.url==='/fixture.js'){response.writeHead(200,{'Content-Type':'text/javascript; charset=utf-8'});response.end(javascript);return;}
  if(request.url==='/client.css'){response.writeHead(200,{'Content-Type':'text/css; charset=utf-8'});response.end(css);return;}
  if(request.url==='/'){response.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});response.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Message behavior fixture</title><link rel="stylesheet" href="/client.css"></head><body><main style="padding:24px;max-width:1100px;margin:auto"><h1>Message behavior fixture</h1><p>Isolated component evidence with explicit protocol fixtures.</p><div id="scene"></div></main><script type="module" src="/fixture.js"></script></body></html>');return;}
  response.writeHead(404);response.end('Unknown fixture route');
 }catch(error){response.writeHead(400,{'Content-Type':'application/json'});response.end(JSON.stringify({message:error.message}));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const origin='http://127.0.0.1:'+server.address().port;
const portServer=createServer();await new Promise(resolve=>portServer.listen(0,'127.0.0.1',resolve));const cdpPort=portServer.address().port;await new Promise(resolve=>portServer.close(resolve));
const moliBin=process.env.MOLI_BIN??path.join(homedir(),'.local/bin/moli');
const version=spawnSync(moliBin,['--version'],{encoding:'utf8'});assert.equal(version.status,0,version.stderr||'Moli unavailable');
const moli=spawn(moliBin,['serve','--host','127.0.0.1','--port',String(cdpPort),'--layout'],{stdio:['ignore','pipe','pipe']});
let moliOutput='';moli.stdout.on('data',chunk=>{moliOutput+=chunk;});moli.stderr.on('data',chunk=>{moliOutput+=chunk;});
const endpoint='http://127.0.0.1:'+cdpPort;
let browser;
const entries=[],failures=[],consoleErrors=[];
const ALL_STATES=['severity-info','severity-success','severity-warning','severity-danger','toast-action','toast-timed','dialog-sm','dialog-md','dialog-lg','dialog-xl','minimal-dialog','confirm-pending','picker','export-empty','export-pending','tooltip','popover','ribbon','permission-dialog','empty','network-offline','network-syncing','network-error','block-delayed','block-nested','effect'];
const ALL_WIDTHS=[{name:'desktop',width:1440,height:1000},{name:'intermediate',width:800,height:1000},{name:'phone',width:390,height:844}];
const states=process.env.MESSAGES_BROWSER_STATES?.split(',')??ALL_STATES;assert.ok(states.length&&states.every(value=>ALL_STATES.includes(value))&&new Set(states).size===states.length,'Invalid state selection');
const widths=process.env.MESSAGES_BROWSER_VIEWPORTS?process.env.MESSAGES_BROWSER_VIEWPORTS.split(',').map(name=>{const viewport=ALL_WIDTHS.find(value=>value.name===name);assert.ok(viewport,'Invalid viewport selection');return viewport;}):ALL_WIDTHS;
const appearances=process.env.MESSAGES_BROWSER_THEMES?.split(',')??['light','dark'];assert.ok(appearances.length&&appearances.every(value=>['light','dark'].includes(value)),'Invalid theme selection');
const freshDocument=process.env.MESSAGES_BROWSER_FRESH_DOCUMENT==='1';
let revision='unverified';try{revision=(await readFile(path.join(root,'REVISION'),'utf8')).trim();}catch{const git=spawnSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'});assert.equal(git.status,0);revision=git.stdout.trim();}
const generated=new Date().toISOString();
try{
 let ready=false;for(let attempt=0;attempt<100;attempt++){if(moli.exitCode!==null)throw new Error('Moli exited: '+moliOutput);try{const response=await fetch(endpoint+'/json/version');if(response.ok){ready=true;break;}}catch{}await new Promise(resolve=>setTimeout(resolve,100));}assert.ok(ready,'Moli discovery did not become ready');
 browser=await chromium.connectOverCDP(endpoint);const context=browser.contexts()[0];assert.ok(context,'Moli did not expose a browser context');const page=await context.newPage();page.setDefaultTimeout(5000);page.setDefaultNavigationTimeout(15000);
 page.on('pageerror',error=>consoleErrors.push(error.message));page.on('console',message=>{if(message.type()==='error')consoleErrors.push(message.text());});
 for(const appearance of appearances)for(const viewport of widths){
  await page.setViewportSize({width:viewport.width,height:viewport.height});await page.goto(origin,{waitUntil:'networkidle'});await page.waitForSelector('body[data-ready="true"]');await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},appearance);
  for(const state of states){
   const name=appearance+'-'+viewport.name+'-'+state;
   let phase='setup';
   try{
    if(freshDocument){await page.goto(origin,{waitUntil:'networkidle'});await page.waitForSelector('body[data-ready="true"]');await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},appearance);}
    await page.evaluate(value=>window.fixture.show(value),state);
    if(process.env.MESSAGES_BROWSER_LAYOUT_PROBE==='1'){
      const geometry=()=>page.evaluate(()=>[...document.querySelectorAll('dialog,button')].map(node=>{const box=node.getBoundingClientRect();return {tag:node.tagName,text:node.textContent?.slice(0,128),x:box.x,y:box.y,width:box.width,height:box.height};}));
      const before=await geometry();await page.screenshot({path:path.join(output,name+'.layout-probe.png'),fullPage:true});await chmod(path.join(output,name+'.layout-probe.png'),0o600);const after=await geometry();await write(name+'.layout-probe.json',JSON.stringify({before,after},null,2)+'\n');
    }
    // Moli's screenshot pipeline publishes layout geometry for newly inserted DOM. The paired
    // before/after evidence is retained in attempt-5-capture; synchronize real rendering before
    // Playwright hit testing, then require positive measured geometry below.
    if(state==='picker')await page.waitForSelector(viewport.width<640?'.select-create-card':'.select-create-list tbody tr',{state:'attached'});
    await page.screenshot({fullPage:true});
    if(state==='tooltip'){await page.getByRole('button',{name:'More information',exact:true}).hover();await page.waitForSelector('.message-tooltip',{state:'attached'});await page.screenshot({fullPage:true});await page.waitForSelector('.message-tooltip');}
    if(state==='confirm-pending'){await page.getByRole('button',{name:'Continue',exact:true}).click();await page.waitForFunction(()=>[...document.querySelectorAll('dialog:not([inert]) button')].every(button=>button.disabled));}
    if(state==='picker')await page.waitForSelector(viewport.width<640?'.select-create-card':'.select-create-list tbody tr');
    if(state==='export-empty')await page.getByRole('button',{name:'Download',exact:true}).click();
    if(state==='export-pending'){await page.getByText('Name',{exact:true}).click();assert.equal(await page.locator('.export-dialog-body input[type="checkbox"]').first().isChecked(),true,'Native label activation did not select the export field');await page.getByRole('button',{name:'Download',exact:true}).click();await page.waitForFunction(()=>document.querySelector('[data-export]').disabled);}
    if(state==='permission-dialog')await page.getByRole('button',{name:'Browser notifications are blocked. Learn how to enable them.',exact:true}).click();
    await page.screenshot({fullPage:true});
    if(state.startsWith('dialog-')){await page.keyboard.press('Tab');await page.keyboard.press('Shift+Tab');assert.equal(await page.evaluate(()=>document.activeElement?.closest('dialog')!==null),true,'Dialog keyboard focus escaped');}
    phase='state-assertions';
    const observed=await page.evaluate(({state,width})=>{
     const query=selector=>document.querySelector(selector),all=selector=>[...document.querySelectorAll(selector)];const fail=message=>{throw new Error(message);};
     if(state.startsWith('severity-')&&query('.client-notification')?.dataset.level!==state.slice(9))fail('Wrong severity');
     if(state==='toast-action'&&!all('.client-notification button').some(button=>button.textContent==='Review result'))fail('Action control missing');
     if(state==='toast-timed'&&all('.client-notification').length!==2)fail('Timed/sticky notice state missing');
     if(state.startsWith('dialog-')){const modal=query('dialog'),body=modal?.querySelector('.message-dialog-body'),box=modal?.getBoundingClientRect(),size=state.slice(7);if(modal?.dataset.size!==size||!modal.querySelector('header')||!modal.querySelector('footer'))fail('Wrong dialog size/header/footer');if(!box||box.width<=0||box.height<=0)fail('Dialog has no measured geometry');if(getComputedStyle(body).overflowY!=='auto')fail('Dialog body is not scrollable');if(width<640){if(['lg','xl'].includes(size)){if(Math.abs(box.width-innerWidth)>1||Math.abs(box.height-innerHeight)>1||Math.abs(box.x)>1||Math.abs(box.y)>1)fail('Large phone dialog is not fullscreen');}else if(box.width>=innerWidth||box.height>=innerHeight)fail('Minimal small/medium phone dialog should remain inset');if(body.scrollHeight<=body.clientHeight)fail('Long phone body did not retain scrollable overflow');}}
     if(state==='minimal-dialog'){const modal=query('dialog');if(!modal||modal.querySelector('header,footer')||modal.getAttribute('aria-label')!=='Minimal notice')fail('Optional header/footer handling failed');} 
     if(state==='confirm-pending'&&(all('dialog').length!==2||!query('dialog[inert]')))fail('Nested modal state missing');
     /// (partial: actual responsive selection/domain/context; saved RecordForm integration is separate)
     if(state==='picker'&&!(width<640?query('.select-create-cards'):query('.select-create-list')))fail('Wrong responsive picker');
     if(state==='export-empty'&&query('.client-notification')?.dataset.level!=='danger')fail('Empty export danger notice missing');
     if(state==='export-pending'&&(!query('[data-export]')?.disabled||!window.fixture.status().downloaded))fail('Export is not pending');
     if(state==='tooltip'&&!query('[role="tooltip"]'))fail('Tooltip missing');if(state==='popover'&&!query('.message-popover'))fail('Popover missing');
     if(state==='ribbon'&&(!query('.message-ribbon-compact')||query('.notification-permission-alert')?.hidden))fail('Ribbon/permission state missing');
     if(state==='permission-dialog'&&!query('dialog')?.textContent.includes('Browser notification permission'))fail('Permission dialog missing');
     /// (partial: actual no-content helper; app loading/error distinction is separate)
     if(state==='empty'&&query('.message-no-content')?.getAttribute('role')!=='alert')fail('No-content helper missing');
     if(state.startsWith('network-')&&query('.client-network-status')?.dataset.state!==state.slice(8))fail('Wrong network state');
     if(state==='block-delayed'&&query('.client-block-ui')?.dataset.visible!=='false')fail('Block delay not retained');
     if(state==='block-nested'&&query('.client-block-ui')?.dataset.visible!=='true')fail('Visible nested block missing');
     if(state==='effect'&&!query('.client-effect'))fail('Effect missing');
     const surface=state.startsWith('severity-')||state.startsWith('toast-')?query('.client-notification'):state==='tooltip'?query('.message-tooltip'):state==='popover'?query('.message-popover'):state==='ribbon'?query('.message-ribbon'):state==='empty'?query('.message-no-content'):state.startsWith('network-')?query('.client-network-status'):state.startsWith('block-')?query('.client-block-ui'):state==='effect'?query('.client-effect'):undefined;if(surface){const box=surface.getBoundingClientRect();if(box.width<=0||box.height<=0)fail('Expected surface has no rendered geometry');}
     const dialog=query('dialog:not([inert])');if(dialog){const box=dialog.getBoundingClientRect();if(box.width<=0||box.height<=0)fail('Active dialog geometry is empty');if(box.width>innerWidth+1||box.left< -1||box.right>innerWidth+1)fail('Dialog exceeds viewport');}
     if(document.documentElement.scrollWidth>innerWidth+1)fail('Horizontal overflow');
     return {title:query('dialog:not([inert]) h2')?.textContent??null,notices:all('.client-notification').map(node=>({level:node.dataset.level,text:node.textContent})),dialogs:all('dialog').map(node=>({size:node.dataset.size,inert:node.inert,open:node.open})),status:window.fixture.status()};
    },{state,width:viewport.width});
    assert.deepEqual(observed.status.errors,[],'Component error presenter received failures');
    phase='screenshot';
    const screenshot=name+'.png';await page.screenshot({path:path.join(output,screenshot),fullPage:true});await chmod(path.join(output,screenshot),0o600);
    phase='axe';
    let axe;try{axe=await auditPage(page);}catch(error){const raw=await page.evaluate(async()=>window.axe.run(document));await write(name+'.axe.json',JSON.stringify(raw,null,2)+'\n');throw error;}
    await write(name+'.axe.json',JSON.stringify(axe,null,2)+'\n');
    phase='interaction-assertions';
    if(state==='minimal-dialog'){await page.keyboard.press('Escape');await page.waitForFunction(()=>document.querySelectorAll('dialog').length===0);}
    if(state.startsWith('dialog-')){if(state==='dialog-md')await page.keyboard.press('Control+Enter');else await page.getByRole('button',{name:'Close dialog',exact:true}).click();assert.equal(await page.locator('dialog').count(),0);assert.equal(await page.evaluate(()=>document.activeElement?.textContent),'Open dialog');}
    if(state==='toast-action'){await page.getByRole('button',{name:'Review result',exact:true}).click();assert.equal((await page.evaluate(()=>window.fixture.status())).actions,1);await page.getByRole('button',{name:'Close notification',exact:true}).click();assert.equal(await page.locator('.client-notification').count(),0);}
    if(state==='toast-timed'){await page.evaluate(()=>document.querySelector('.client-notification').dispatchEvent(new MouseEvent('mouseleave')));await page.waitForFunction(()=>document.querySelectorAll('.client-notification').length===1);}
    if(state==='confirm-pending'){await page.evaluate(()=>window.fixture.resolve());await page.waitForFunction(()=>document.querySelectorAll('dialog').length===1);assert.equal(await page.locator('dialog[inert]').count(),0);}
    if(state==='picker'){await page.getByRole('button',{name:'Fixture partner',exact:true}).click();await page.waitForFunction(()=>window.fixture.status().selection?.id===4);assert.equal(await page.locator('dialog').count(),0);}
    if(state==='export-pending'){await page.evaluate(()=>window.fixture.resolve());await page.waitForFunction(()=>!document.querySelector('[data-export]').disabled);}
    if(state==='block-nested'){await page.evaluate(()=>window.fixture.release(0));assert.equal((await page.evaluate(()=>window.fixture.status())).blocked,true);await page.evaluate(()=>window.fixture.release(1));assert.equal((await page.evaluate(()=>window.fixture.status())).blocked,false);}
    entries.push({name,state,appearance,viewport,screenshot,sha256:hash(await readFile(path.join(output,screenshot))),observed,axe:name+'.axe.json'});
   }catch(error){
    const diagnostic=await page.evaluate(()=>{const inspect=node=>{const style=getComputedStyle(node),rect=node.getBoundingClientRect();return {tag:node.tagName,text:node.textContent?.slice(0,512),open:node.open??null,inert:node.inert,hidden:node.hidden,ariaHidden:node.getAttribute('aria-hidden'),display:style.display,visibility:style.visibility,opacity:style.opacity,rect:{x:rect.x,y:rect.y,width:rect.width,height:rect.height},scrollWidth:node.scrollWidth,clientWidth:node.clientWidth,scrollHeight:node.scrollHeight,clientHeight:node.clientHeight};};return {viewport:{width:innerWidth,height:innerHeight},documentWidth:document.documentElement.scrollWidth,activeElement:document.activeElement?inspect(document.activeElement):null,dialogs:[...document.querySelectorAll('dialog')].map(inspect),buttons:[...document.querySelectorAll('button')].map(inspect),inputs:[...document.querySelectorAll('input')].map(inspect),scene:document.querySelector('#scene')?.outerHTML.slice(0,16384),errors:window.fixture?.status().errors};});
    await write(name+'.failure.dom.json',JSON.stringify(diagnostic,null,2)+'\n');
    await page.screenshot({path:path.join(output,name+'.failure.png'),fullPage:true});await chmod(path.join(output,name+'.failure.png'),0o600);
    await write(name+'.after-screenshot.geometry.json',JSON.stringify(await page.evaluate(()=>[...document.querySelectorAll('dialog,button')].map(node=>{const box=node.getBoundingClientRect();return {tag:node.tagName,text:node.textContent?.slice(0,128),x:box.x,y:box.y,width:box.width,height:box.height};})),null,2)+'\n');
    if(phase!=='axe'){try{const failedStateAxe=await auditPage(page);await write(name+'.failure.axe.json',JSON.stringify(failedStateAxe,null,2)+'\n');}catch(auditError){const raw=await page.evaluate(async()=>window.axe.run(document));await write(name+'.failure.axe.json',JSON.stringify(raw,null,2)+'\n');diagnostic.auditError=auditError.message;}}
    failures.push({name,phase,message:error.message,diagnostic:name+'.failure.dom.json',screenshot:name+'.failure.png'});await write('partial-manifest.json',JSON.stringify({entries,failures},null,2)+'\n');
   }
  }
 }
 let reduced;
 try{await page.emulateMedia({reducedMotion:'reduce'});await page.evaluate(()=>window.fixture.show('effect'));await page.screenshot({fullPage:true});await page.waitForSelector('.client-effect');
 reduced=await page.locator('.client-effect').evaluate(node=>({reduced:node.classList.contains('reduced-motion'),animation:getComputedStyle(node).animationName}));assert.equal(reduced.reduced,true);assert.equal(reduced.animation,'none');
 await page.screenshot({path:path.join(output,'reduced-motion.png'),fullPage:true});await chmod(path.join(output,'reduced-motion.png'),0o600);
 }catch(error){failures.push({name:'reduced-motion',phase:'reduced-motion',message:error.message});}
 await write('manifest.json',JSON.stringify({schema:1,generated_at:generated,host:hostname(),node:process.version,moli:version.stdout.trim(),playwright:require(path.join(homedir(),'.local/share/erpforall-qa/node_modules/playwright/package.json')).version,browser:browser.version(),revision,states,appearances,widths,freshDocument,completeAcceptance:states.length===ALL_STATES.length&&appearances.length===2&&widths.length===3&&!freshDocument&&failures.length===0,origin,cdp_endpoint:endpoint,fixture_entry_sha256:hash(entry),javascript_sha256:hash(javascript),css_sha256:hash(css),sourceFiles,rpcCalls,entries,reducedMotion:reduced,consoleErrors,failures,boundary:'Isolated compiled-source and HTTP protocol fixtures; no installed backend, authentication, persistence, real permission grant, screen-reader speech or deployment acceptance.'},null,2)+'\n');
 console.log(JSON.stringify({host:hostname(),revision,entries:entries.length,failures:failures.length,output}));
 assert.deepEqual(consoleErrors,[],'Browser console/page errors');assert.deepEqual(failures,[],'Message browser states failed; see manifest and raw axe outputs.');
}finally{
 const cleanupErrors=[];
 try{if(browser)await browser.close();}catch(error){cleanupErrors.push(error.message);}
 try{await new Promise((resolve,reject)=>server.close(error=>error?reject(error):resolve()));}catch(error){cleanupErrors.push(error.message);}
 if(moli.exitCode===null){moli.kill('SIGTERM');await Promise.race([new Promise(resolve=>moli.once('exit',resolve)),new Promise(resolve=>setTimeout(resolve,3000))]);if(moli.exitCode===null)moli.kill('SIGKILL');}
 await write('moli.log',moliOutput);if(cleanupErrors.length)throw new Error('Browser fixture cleanup failed: '+cleanupErrors.join('; '));
}

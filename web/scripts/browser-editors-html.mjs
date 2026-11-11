/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled reusable-component acceptance. No server sanitizer, field-write or backend parity proof.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFile, writeFile, mkdir, stat } from 'node:fs/promises';
import { hostname, homedir } from 'node:os';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { auditPage } from '../testing/axe.ts';
const root=fileURLToPath(new URL('../../',import.meta.url)), output=process.env.HTML_BROWSER_OUTPUT;
const suite=process.env.HTML_BROWSER_SUITE??'full';assert.ok(['full','visual'].includes(suite),'HTML_BROWSER_SUITE must be full or visual.');
const engine=process.env.HTML_BROWSER_ENGINE??'chromium';assert.equal(engine,'chromium','HTML UI checks require Playwright Chromium.');
const selectionMode=process.env.HTML_BROWSER_SELECTION_MODE??'native';assert.equal(selectionMode,'native','HTML UI checks require native selection.');
assert.ok(output,'Set HTML_BROWSER_OUTPUT to a task-owned evidence directory.');
await mkdir(output,{recursive:true,mode:0o700});
const qa=path.join(homedir(),'.local/share/erpforall-qa');
assert.equal((await stat(qa)).mode&0o077,0,'Browser tooling directory must be private.');
const require=createRequire(import.meta.url), {chromium}=require(path.join(qa,'node_modules/playwright'));
const {build}=require(path.join(root,'web/node_modules/esbuild'));
const hash=value=>createHash('sha256').update(value).digest('hex');
const client=name=>JSON.stringify(path.join(root,'addons/web/static/src/client',name));
const entry=`
import {createHtmlEditor,MessageServices} from ${client('public.ts')};
import {markHtmlEditorTransient} from ${client('html-editor-document.ts')};
let handle,services,changes=[],errors=[],resolvePending,rejectPending,keyEvents=[];
document.addEventListener("keydown",event=>{keyEvents.push({key:event.key,ctrl:event.ctrlKey,meta:event.metaKey,shift:event.shiftKey,selection:document.getSelection()?.toString(),anchor:document.getSelection()?.anchorOffset,focus:document.getSelection()?.focusOffset,target:event.target?.className});if(keyEvents.length>30)keyEvents.shift();},true);
const scene=document.querySelector('#scene');
const report=e=>errors.push({kind:e?.kind,message:e instanceof Error?e.message:String(e)});
function show(state='default'){
 handle?.dispose();services?.dispose();scene.replaceChildren();changes=[];errors=[];keyEvents=[];resolvePending=undefined;rejectPending=undefined;
 services=new MessageServices(scene,report);
 handle=createHtmlEditor({environment:{messages:services,report},label:'Rich notes',value:state==='empty'||state==='required'?'':state==='blocks'?'<h1>Title</h1><p>Body</p>':'<p>Hello world</p>',readonly:state==='readonly',disabled:state==='disabled',required:state==='required',invalid:state==='invalid'?'Notes need correction.':undefined,clipboardMode:state==='plain'?'plain-text':'rich',change:html=>{changes.push(html);if(state==='pending')return new Promise((resolve,reject)=>{resolvePending=resolve;rejectPending=reject;});if(state==='error')return Promise.reject(new Error('Owner refused draft.'));}});
 scene.append(handle.node);
 if(state==='protected'){const island=handle.editable.querySelector('p');markHtmlEditorTransient(island);island.contentEditable='false';}
}
function select(from,to=from){const root=handle.editable;root.focus();const walker=document.createTreeWalker(root,NodeFilter.SHOW_TEXT);let nodes=[],node;while(node=walker.nextNode())nodes.push(node);const point=offset=>{for(const n of nodes){if(offset<=n.length)return[n,offset];offset-=n.length;}return[root,root.childNodes.length];};const a=point(from),b=point(to);document.getSelection().setBaseAndExtent(a[0],a[1],b[0],b[1]);document.dispatchEvent(new Event('selectionchange'));}
function paste(values,files=false){const transfer=new DataTransfer();for(const [type,value]of Object.entries(values))transfer.setData(type,value);if(files)transfer.items.add(new File(['x'],'image.png',{type:'image/png'}));return !handle.editable.dispatchEvent(new ClipboardEvent('paste',{bubbles:true,cancelable:true,clipboardData:transfer}));}
window.fixture={type:text=>{const root=handle.editable;root.focus();const selection=document.getSelection(),range=document.createRange();range.selectNodeContents(root);range.collapse(false);selection.removeAllRanges();selection.addRange(range);root.dispatchEvent(new InputEvent('beforeinput',{bubbles:true,cancelable:true,inputType:'insertText',data:text}));range.insertNode(document.createTextNode(text));range.collapse(false);selection.removeAllRanges();selection.addRange(range);root.dispatchEvent(new InputEvent('input',{bubbles:true,inputType:'insertText',data:text}));},debug:()=>({keyEvents,active:document.activeElement?.outerHTML,selection:{text:document.getSelection()?.toString(),anchor:document.getSelection()?.anchorOffset,focus:document.getSelection()?.focusOffset},html:handle.editable.outerHTML}),show,select,paste,editable:()=>handle.editable,value:()=>handle.exportHtml(),state:()=>({changes,errors,...handle.state()}),set:value=>handle.setValue(value),dispose:()=>handle.dispose(),complete:()=>resolvePending?.(),fail:()=>rejectPending?.(new Error('Owner refused draft.'))};
show();document.body.dataset.ready='true';
`;
const bundle=await build({stdin:{contents:entry,resolveDir:root,sourcefile:'html-browser-fixture.ts',loader:'ts'},bundle:true,write:false,platform:'browser',format:'esm',target:'es2023',metafile:true,alias:{'@erpforall/elang':path.join(root,'web/src/index.ts'),'@erpforall/expr':path.join(root,'web/packages/expr/src/index.ts')}});
const javascript=bundle.outputFiles[0].contents,css=await readFile(path.join(root,'addons/web/static/src/client/client.css'));
const sources=[];
for(const filename of Object.keys(bundle.metafile.inputs).sort()){
 if(filename==='html-browser-fixture.ts'||filename.includes('node_modules'))continue;
 const absolute=path.resolve(root,filename);sources.push({path:path.relative(root,absolute),sha256:hash(await readFile(absolute))});
}
for(const filename of ['addons/web/static/src/client/client.css','web/scripts/browser-editors-html.mjs','web/testing/axe.ts','web/pnpm-lock.yaml'])sources.push({path:filename,sha256:hash(await readFile(path.join(root,filename)))});
const revision=(await readFile(path.join(root,'REVISION'),'utf8')).trim();
const server=createServer((request,response)=>{
 if(request.url==='/fixture.js'){response.writeHead(200,{'Content-Type':'text/javascript; charset=utf-8'});response.end(javascript);}
 else if(request.url==='/client.css'){response.writeHead(200,{'Content-Type':'text/css; charset=utf-8'});response.end(css);}
 else if(request.url==='/'){response.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});response.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>HTML editor acceptance</title><link rel="stylesheet" href="/client.css"></head><body><main style="padding:24px;max-width:900px;margin:auto"><h1>Rich text editor</h1><p>Reusable component acceptance; record storage is outside this fixture.</p><section id="scene" aria-label="Editor fixture"></section><button id="after" type="button">Next field</button></main><script type="module" src="/fixture.js"></script></body></html>');}
 else if(request.url==='/favicon.ico'){response.writeHead(204);response.end();}
 else {response.writeHead(404);response.end('Unexpected resource');}
});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
const origin='http://127.0.0.1:'+server.address().port;
const result={generated_at:new Date().toISOString(),host:hostname(),node:process.version,command:process.argv,revision,browser_transport:'Playwright Chromium',suite,selection_mode:selectionMode,excluded_checks:suite==='visual'?['Native editor interactions (visual states use explicit programmatic input)']:[],boundary:'Compiled component fixture; no server sanitizer, backend, field adapter or durable-write acceptance',bundle_sha256:hash(javascript),sources,checks:[],captures:[],errors:[],unexpected_requests:[]};
let browser,page;
try{
 browser=await chromium.launch({headless:true});result.browser=browser.version();
 const context=browser.contexts()[0]??await browser.newContext();page=await context.newPage();await page.setViewportSize({width:1280,height:900});page.setDefaultTimeout(8000);page.setDefaultNavigationTimeout(15000);
 page.on('pageerror',error=>result.errors.push(error.message));page.on('console',message=>{if(message.type()==='error')result.errors.push(message.text());});
 page.on('request',request=>{if(!request.url().startsWith(origin+'/'))result.unexpected_requests.push(request.url());});
 await page.goto(origin);await page.waitForSelector('body[data-ready="true"]');
 const show=state=>page.evaluate(value=>window.fixture.show(value),state),value=()=>page.evaluate(()=>window.fixture.value()),state=()=>page.evaluate(()=>window.fixture.state());
 const select=async(from,to=from)=>{await page.evaluate(([a,b])=>window.fixture.select(a,b),[from,to]);await page.screenshot({caret:'initial'});};
 const paste=(values,files=false)=>page.evaluate(([data,hasFiles])=>window.fixture.paste(data,hasFiles),[values,files]);
 if(suite==='full'){await select(0,5);await page.getByRole('button',{name:'Bold',exact:true}).click();assert.equal(await value(),'<p><strong>Hello</strong> world</p>');assert.equal((await state()).changes.length,1);
 await select(1,4);await page.getByRole('button',{name:'Bold',exact:true}).click();assert.equal(await value(),'<p><strong>H</strong>ell<strong>o</strong> world</p>');result.checks.push('Pointer bold/unbold preserves surrounding runs');
 await show('default');await select(0);if(selectionMode==='native'){await page.keyboard.press('Shift+ArrowRight');await page.keyboard.press('Shift+ArrowRight');}else await select(0,2);result.keyboard_before=await page.evaluate(()=>window.fixture.debug());await page.keyboard.press('Control+b');assert.equal(await value(),'<p><strong>He</strong>llo world</p>');result.checks.push(selectionMode==='native'?'Real keyboard selection and formatting shortcut':'Explicit-range formatting shortcut; native selection route excluded');
 await show('default');await select(6,11);await paste({'text/html':'<b>Earth</b>','text/plain':'wrong'});assert.equal(await value(),'<p>Hello <strong>Earth</strong></p>');
 await page.keyboard.press('Control+z');assert.equal(await value(),'<p>Hello world</p>');const undoSelection=await page.evaluate(()=>document.getSelection().toString());if(selectionMode==='native')assert.equal(undoSelection,'world');else if(undoSelection!=='world'){result.excluded_checks.push('Native history-key selection restoration');result.native_history_selection={actual:undoSelection,expected:'world'};}
 await page.keyboard.press('Control+y');assert.equal(await value(),'<p>Hello <strong>Earth</strong></p>');
 await page.keyboard.press('Meta+z');assert.equal(await value(),'<p>Hello world</p>');await page.keyboard.press('Meta+Shift+z');assert.equal(await value(),'<p>Hello <strong>Earth</strong></p>');result.checks.push('HTML clipboard one-step history and platform modifier paths');
 await show('plain');await select(0,11);await paste({'text/html':'<b>rich</b>','text/plain':'<literal>\nline'});assert.equal(await value(),'<p>&lt;literal&gt;<br>line</p>');result.checks.push('Plain destination keeps literal text and line breaks');
 await show('empty');await select(0);await paste({'application/vnd.erpforall.html-editor+json':JSON.stringify({version:1,html:'<b>Own</b>'})});assert.equal(await value(),'<strong>Own</strong>');
 await paste({'application/vnd.erpforall.html-editor+json':JSON.stringify({version:2,html:'bad'})});assert.equal((await state()).errors.at(-1).message,'Invalid ERPForAll HTML clipboard payload.');result.checks.push('Editor-origin MIME receives strict version/admission checks');
 await show('empty');await select(0);await paste({'text/html':'<p onmouseover="alert(1)">safe<a href="https://attacker.invalid/">link</a><img src="https://attacker.invalid/pixel"><script>alert(1)</script><svg onload="alert(1)"></svg></p>'});assert.equal(await value(),'<p>safelink</p>');assert.equal(await page.locator('#scene img,#scene script,#scene svg,#scene a').count(),0);assert.equal(await page.locator('.html-editor-status').textContent(),'Pasted formatting was simplified.');result.checks.push('Hostile paste renders only reconstructed admitted structure');
 await show('protected');await select(0,5);assert.equal(await paste({'text/plain':'changed'}),true);assert.equal((await state()).canUndo,false);assert.equal(await value(),'<p>Hello world</p>');assert.deepEqual((await state()).changes,[]);result.checks.push('Protected selection cannot authorize paste');
 await show('default');await select(0,5);await page.getByRole('button',{name:'Bold',exact:true}).focus();await page.keyboard.press('Enter');assert.equal(await value(),'<p><strong>Hello</strong> world</p>');await page.evaluate(()=>window.fixture.set('<p>Parent replacement</p>'));assert.equal((await state()).canUndo,false);assert.equal((await state()).dirty,false);result.checks.push('Keyboard toolbar and silent owner baseline replacement');
 await show('pending');await select(11);await page.keyboard.type('!');assert.equal((await state()).pending,true);await page.evaluate(()=>window.fixture.set('<p>New owner</p>'));await page.evaluate(()=>window.fixture.fail());assert.equal(await value(),'<p>New owner</p>');assert.deepEqual((await state()).errors,[]);result.checks.push('Replacement invalidates pending callback completion');
 }
 if(suite==='full'){
  // Block merge: Backspace at the start of a paragraph after a heading must either merge cleanly or be refused; never leave the editor locked.
  await show('blocks');await page.evaluate(()=>{const p=window.fixture.editable().querySelector('p'),range=document.createRange();window.fixture.editable().focus();range.setStart(p.firstChild,0);range.collapse(true);const selection=document.getSelection();selection.removeAllRanges();selection.addRange(range);});
  await page.keyboard.press('Backspace');
  const merged=await value(),mergeState=await state();
  assert.ok(mergeState.errors.length===0&&merged==='<h1>TitleBody</h1>'||mergeState.errors.length>0&&merged==='<h1>Title</h1><p>Body</p>','Backspace block merge neither merges cleanly nor refuses: '+merged);
  await page.keyboard.press('Control+z');await page.keyboard.press('Control+y');
  assert.equal((await state()).errors.length,mergeState.errors.length,'Undo and redo after a block merge report no new error');
  result.checks.push('Backspace block merge into a heading stays editable');
  // Unrouted native format commands (italic, underline) are blocked rather than written by the engine.
  await show('default');await select(0,5);await page.keyboard.press('Control+i');await page.keyboard.press('Control+u');await page.keyboard.press('Meta+i');await page.keyboard.press('Meta+u');
  assert.equal(await value(),'<p>Hello world</p>');assert.deepEqual((await state()).errors,[]);result.checks.push('Native italic and underline commands are blocked');
 }
 const allScenes=['default','hover','focus','active','readonly','disabled','invalid','required','empty','dirty','pending','error'];
 for(const theme of ['light','dark'])for(const viewport of [{name:'desktop',width:1280,height:900},{name:'intermediate',width:820,height:900},{name:'phone',width:390,height:844}]){
  await page.setViewportSize({width:viewport.width,height:viewport.height});await page.emulateMedia({reducedMotion:'reduce'});await page.evaluate(value=>document.documentElement.dataset.theme=value,theme);
  for(const scene of allScenes){
   result.current={theme,viewport,scene};await show(['hover','focus','active','dirty'].includes(scene)?'default':scene);
   if(['hover','focus','active'].includes(scene))await select(0,5);
   if(scene==='hover')await page.getByRole('button',{name:'Bold',exact:true}).hover();
   if(scene==='focus')await page.getByRole('textbox',{name:'Rich notes',exact:true}).focus();
   if(scene==='active')await page.getByRole('button',{name:'Bold',exact:true}).click();
   if(['dirty','pending','error'].includes(scene)){await select(11);if(suite==='visual')await page.evaluate(()=>window.fixture.type('!'));else await page.keyboard.type('!');if(scene==='error')await page.waitForFunction(()=>window.fixture.state().errors.length===1);}
   // Publish the current layout before reading geometry.
   await page.screenshot({caret:'initial'});
   const geometry=await page.evaluate(()=>({width:innerWidth,scrollWidth:document.documentElement.scrollWidth,buttons:[...document.querySelectorAll('#scene button')].filter(n=>!n.hidden&&getComputedStyle(n).display!=='none').map(n=>{const r=n.getBoundingClientRect();return {label:n.textContent,width:r.width,height:r.height};})}));
   assert.ok(geometry.scrollWidth<=viewport.width+1,'No horizontal overflow');for(const button of geometry.buttons)assert.ok(button.width>=24&&button.height>=24,'Every visible command has a 24px target');
   if(scene==='readonly'||scene==='disabled'){assert.equal(await page.getByRole('textbox',{name:'Rich notes',exact:true}).getAttribute('contenteditable'),'false');assert.equal(await page.locator('#scene button:not(:disabled)').count(),0);}
   if(scene==='invalid')assert.equal(await page.getByRole('textbox',{name:'Rich notes',exact:true}).getAttribute('aria-invalid'),'true');
   const axe=await auditPage(page);assert.deepEqual(axe.violations,[]);assert.deepEqual(axe.incomplete,[],'Incomplete accessibility findings need explicit investigation');
   const name=`${theme}-${viewport.name}-${scene}`,png=await page.screenshot({fullPage:true,caret:'initial'});await writeFile(path.join(output,name+'.png'),png);await writeFile(path.join(output,name+'-axe.json'),JSON.stringify(axe,null,2));result.captures.push({name,theme,viewport,scene,sha256:hash(png),geometry});
  }
 }
 assert.deepEqual(result.unexpected_requests,[]);assert.deepEqual(result.errors,[]);result.status=result.excluded_checks.length?'partial':'passed';
}catch(error){result.status='failed';if(page){try{result.debug=await page.evaluate(()=>window.fixture?.debug());}catch(debugError){result.debug_error=String(debugError);}}result.failure=error instanceof Error?error.stack:String(error);if(page){try{await page.screenshot({path:path.join(output,'failure.png'),fullPage:true,caret:'initial'});await writeFile(path.join(output,'failure.html'),await page.content());}catch(captureError){result.capture_error=String(captureError);}}throw error;}
finally{
 if(browser)await browser.close();
 server.closeAllConnections();await new Promise((resolve,reject)=>server.close(error=>error?reject(error):resolve()));
 await writeFile(path.join(output,'manifest.json'),JSON.stringify(result,null,2));
}

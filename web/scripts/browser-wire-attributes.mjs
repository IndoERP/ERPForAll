/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shipped compiled app consuming an actual caller-authorized native get_views capture.
// RPC record/action responses below are fixtures; native auth evidence is a separate test receipt.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {createHash} from 'node:crypto';
import {readFile,writeFile,mkdir,lstat,realpath} from 'node:fs/promises';
import {hostname,homedir} from 'node:os';
import {fileURLToPath,pathToFileURL} from 'node:url';
import path from 'node:path';
const root=fileURLToPath(new URL('../../',import.meta.url));
const capture=process.env.ERP_H18_CAPTURE_DIR, output=process.env.WIRE_BROWSER_OUTPUT;
assert.ok(capture && output,'Set ERP_H18_CAPTURE_DIR and WIRE_BROWSER_OUTPUT.');
await mkdir(output);
const qaDir=path.join(homedir(),'.local/share/erpforall-qa'),qaInfo=await lstat(qaDir);
assert.ok(qaInfo.isDirectory() && (qaInfo.mode & 0o777)===0o700 && qaInfo.uid===process.getuid(),'Playwright QA directory must be operator-owned and mode 700');
const playwrightPath=await realpath(path.join(qaDir,'node_modules/playwright/index.mjs'));
assert.ok(playwrightPath.startsWith((await realpath(qaDir))+path.sep),'Playwright must resolve inside the private QA directory');
const {chromium}=await import(pathToFileURL(playwrightPath).href);
const playwrightVersion=JSON.parse(await readFile(path.join(path.dirname(playwrightPath),'package.json'),'utf8')).version;
assert.equal(typeof playwrightVersion,'string','Playwright package must declare its version');
const captureBytes=await readFile(path.join(capture,'wire-attrs-host.json'));
const captureReceipt=await readFile(path.join(capture,'..','run.log'),'utf8');
const captureRevision=/^revision: (.+)$/m.exec(captureReceipt)?.[1];assert.ok(captureRevision,'Native capture requires its run receipt');
const description=JSON.parse(captureBytes);
const form=description.views.find(view=>view.type==='form'), list=description.views.find(view=>view.type==='list');
assert.ok(form && list,'The native capture must include both actual views.');
const object=form.nodes.find(node=>node.kind==='button' && node.name==='action_archive');
const actionNode=form.nodes.find(node=>node.kind==='action_button' && node.target.kind==='action');
const inert=form.nodes.find(node=>node.kind==='action_button' && node.target.kind==='inert');
const ribbon=form.nodes.find(node=>node.kind==='ribbon');
assert.ok(object && actionNode && inert && ribbon);
const launcherName=object.label || object.details.title;
const actionId=actionNode.target.id, model=description.model, revision=description.registry_version;
const manifest=JSON.parse(await readFile(path.join(root,'web/dist/human-ui/manifest.json'),'utf8'));
const assets=new Map();
for(const asset of manifest.assets){const data=await readFile(path.join(root,'web/dist/human-ui',asset.path));assert.equal(createHash('sha256').update(data).digest('hex'),asset.sha256);assets.set('/erp/assets/'+asset.path,{data,type:asset.content_type});}
for(const name of ['logo.svg','logo-dark.svg'])assets.set('/erp/brand/'+name,{data:await readFile(path.join(root,'crates/erp-server/brand/erpforall-'+name)),type:'image/svg+xml'});
const js=manifest.assets.find(asset=>asset.name==='client.js'),css=manifest.assets.find(asset=>asset.name==='client.css');
const bootstrap={wire_major:1,registry_version:revision,asset_version:'wire-fixture',identity:{uid:2,name:'Fixture user',internal:true},companies:[{id:1,name:'Fixture company'}],current_company_id:1,allowed_company_ids:[1],active_company_ids:[1],locale:{lang:'en_US',tz:'UTC',date_format:'%m/%d/%Y',time_format:'%H:%M:%S'},csrf:{logout:'fixture-not-secret'}};
const calls=[],errors=[],checks=[],screenshots=[];
let serverError,persisted,invocationGate;
function resetRecord(){persisted={id:5,name:'Before',body:false,active:true,priority:description.fields[model].priority.selection[0][0]};}
resetRecord();
const action={wire_major:1,id:actionId,name:'Notes',model,target:'current',views:[{id:list.identity.id,type:'list'},{id:form.identity.id,type:'form'}],domain:null,context:null};
const server=createServer(async(req,res)=>{
 try{
  const pathname=new URL(req.url,'http://fixture').pathname,asset=assets.get(pathname);
  if(asset){res.setHeader('Content-Type',asset.type);res.end(asset.data);return;}
  if(req.method==='GET' && (pathname==='/erp'||pathname===`/erp/action-${actionId}`)){res.setHeader('Content-Type','text/html');res.end(`<!doctype html><html lang="en"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Wire attributes acceptance</title><div id="erp-root"></div><script type="application/json" id="erp-bootstrap">${JSON.stringify(bootstrap)}</script><script type="module" src="/erp/assets/${js.path}"></script></html>`);return;}
  if(pathname==='/favicon.ico'){res.writeHead(204);res.end();return;}
  let input='';for await(const chunk of req){input+=chunk;if(input.length>65536)throw new Error('Oversized fixture request');}
  const body=JSON.parse(input);calls.push({path:pathname,body});let value;
  if(pathname.endsWith('erp_describe'))value={wire_major:1,registry_version:revision,scope:body.scope,declarations:[]};
  else if(pathname.endsWith('erp_read_values'))value={wire_major:1,scope:body.scope,revision:'1',values:[]};
  else if(pathname.endsWith('erp_load_web_menus'))value={wire_major:1,registry_version:revision,roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:'Notes',action_id:actionId,children:[],icon:null}]};
  else if(pathname.endsWith('erp_load_web_action_views'))value={action,views:description};
  else if(pathname.endsWith('erp_load_web_action'))value=action;
  else if(pathname.endsWith('get_views'))value=description;
  else if(pathname.endsWith('web_search_read'))value={length:1,records:[persisted]};
  else if(pathname.endsWith('formatted_read_group')){assert.deepEqual(body.groupby,['priority']);value=[{priority:persisted.priority,__count:1}];}
  else if(pathname.endsWith('search_count'))value=1;
  else if(pathname.endsWith('web_read'))value=[persisted];
  else if(pathname.endsWith('onchange'))value={value:{}};
  else if(pathname.endsWith('web_save')){persisted={...persisted,...body.vals};value=[persisted];}
  else if(pathname.endsWith('action_archive')){assert.ok(invocationGate,'Invocation requires an explicit test release');invocationGate.arrive();await invocationGate.finished;value=false;}
  else if(pathname.endsWith('erp_normalize_action_result'))value={wire_major:1,result:body.result?.type==='ir.actions.act_window'?{kind:'current_window',name:'Notes',model,views:action.views,domain:[],context:{}}:{kind:'none'}};
  else throw new Error(`Unknown fixture RPC: ${pathname}`);
  res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));
 }catch(error){serverError=error;res.writeHead(500,{'Content-Type':'application/json'});res.end(JSON.stringify({message:'Fixture server failure'}));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
let browser,page;
const count=method=>calls.filter(call=>call.path.endsWith('/'+method)).length;
try{
 browser=await chromium.launch({headless:true,...(process.env.CHROME_BIN?{executablePath:process.env.CHROME_BIN}:{})});
 page=await browser.newPage();page.on('pageerror',error=>errors.push(error.message));
 // Emulate browser permission observation without requesting notification permission.
 await page.addInitScript(()=>{let permission='denied';Object.defineProperty(Notification,'permission',{configurable:true,get:()=>permission});window.fixturePermission=value=>{permission=value;window.dispatchEvent(new Event('focus'));};});
 for(const theme of ['light','dark'])for(const width of [1440,820,390]){
  resetRecord();await page.setViewportSize({width,height:900});
  await page.goto(`${origin}/erp/action-${actionId}?id=5&view_type=form&debug=1`);
  const archive=page.getByRole('button',{name:launcherName,exact:true});await archive.waitFor();
  await page.evaluate(theme=>{document.documentElement.dataset.theme=theme;},theme);
  assert.equal(await archive.count(),1,'One accessible declared launcher in the complete mounted app');
  for(const token of ['erp-button-success','erp-button-small','erp-button-circle','addon_archive'])assert.equal(await archive.evaluate((node,token)=>node.classList.contains(token),token),true);
  assert.equal(await archive.locator('svg').count(),1);assert.equal(await archive.getAttribute('title'),object.details.title);
  assert.equal(await page.locator('button[data-focus-key="button:inert"]').isDisabled(),true);
  assert.equal(await page.locator('.message-ribbon').textContent(),ribbon.text);assert.equal(await page.locator('.message-ribbon').getAttribute('title'),ribbon.tooltip);assert.equal(await page.locator('.message-ribbon').getAttribute('data-level'),'success');
  const permission=page.getByRole('button',{name:'Browser notifications are blocked. Learn how to enable them.',exact:true});await permission.waitFor();await permission.click();
  const guidance=page.getByRole('dialog');await guidance.waitFor();await page.keyboard.press('Escape');await guidance.waitFor({state:'hidden'});
  await page.evaluate(()=>window.fixturePermission('granted'));assert.equal(await permission.count(),0);await page.evaluate(()=>window.fixturePermission('denied'));await permission.waitFor();
  // Focus shows the same safe help/debug content as hover.
  await archive.focus();const tooltip=page.getByRole('tooltip');await tooltip.waitFor();assert.ok((await tooltip.textContent()).includes(object.details.help));assert.ok((await tooltip.textContent()).includes(`Model: ${model}`));
  await page.keyboard.press('Tab');await page.keyboard.down('Alt');assert.ok(await page.locator('.button-hotkey-hint').count()>0);await page.keyboard.up('Alt');
  const before=count('action_archive');
  const box=await archive.boundingBox();assert.ok(box && box.width>0 && box.height>0);
  await page.mouse.move(box.x+box.width/2,box.y+box.height/2);await page.mouse.down();
  assert.equal(await archive.evaluate(node=>node.matches(':active')),true,'The native circle has a real pressed state');
  const pressed=`${theme}-${width}-pressed.png`;await page.screenshot({path:path.join(output,pressed),fullPage:true});screenshots.push(pressed);await page.mouse.up();
  let confirm=page.getByRole('dialog',{name:object.details.confirm_title,exact:true});await confirm.waitFor();await confirm.locator('.message-dialog-footer button').nth(1).click();assert.equal(count('action_archive'),before);
  // Quiet windows are measured with the browser's real clock; this is functional debounce proof.
  await page.waitForTimeout(object.details.debounce_ms+20);
  await page.locator('input[name="name"]').fill('Saved by declared action');
  // The existing editor commits on change/blur. Wait for its onchange owner before activation.
  await Promise.all([page.waitForResponse(response=>response.url().endsWith('/onchange')),page.keyboard.press('Tab')]);
  await page.waitForFunction(()=>!document.querySelector('input[name="name"]')?.disabled);
  let arrive,release;const arrived=new Promise(resolve=>{arrive=resolve;});const finished=new Promise(resolve=>{release=resolve;});invocationGate={arrive,release,finished};
  await archive.focus();await page.keyboard.press('Alt+Shift+A');confirm=page.getByRole('dialog',{name:object.details.confirm_title,exact:true});await confirm.waitFor();await confirm.locator('.message-dialog-footer button').nth(0).click();
  let timeout;try{await Promise.race([arrived,new Promise((_,reject)=>{timeout=setTimeout(()=>reject(new Error('Object invocation did not reach the fixture')),30000);})]);}finally{clearTimeout(timeout);}
  assert.equal(await page.locator('.client-block-ui').isVisible(),true);assert.equal(await archive.isDisabled(),true);release();invocationGate=undefined;
  await page.waitForFunction(()=>window.document.querySelectorAll('dialog[open]').length===0);await page.waitForFunction(()=>!document.querySelector('.client-block-ui'));
  await page.waitForTimeout(50);assert.equal(count('action_archive'),before+1);assert.equal(persisted.name,'Saved by declared action');
  const invocation=calls.findLast(call=>call.path.endsWith('/action_archive'));assert.equal(invocation.body.context.default_priority,persisted.priority);if(Object.hasOwn(invocation.body.context,'default_body'))assert.equal(invocation.body.context.default_body,false);assert.deepEqual(invocation.body.ids,[5]);
  const saveIndex=calls.findLastIndex(call=>call.path.endsWith('/web_save')),callIndex=calls.lastIndexOf(invocation);assert.ok(saveIndex<callIndex,'Save precedes object invocation');
  const file=`${theme}-${width}.png`;await page.screenshot({path:path.join(output,file),fullPage:true});screenshots.push(file);checks.push({theme,width,launcher:launcherName,objectCalls:count('action_archive')-before,persistedName:persisted.name});
  if(width>700){const beforeAction=count('erp_load_web_action');await page.getByRole('button',{name:actionNode.label,exact:true}).click();await page.waitForURL(url=>url.searchParams.has('method_action'));assert.equal(count('erp_load_web_action'),beforeAction+1);const loaded=calls.findLast(call=>call.path.endsWith('/erp_load_web_action'));assert.deepEqual(loaded.body.ids,[actionId]);assert.deepEqual(loaded.body.context.active_ids,[5]);assert.equal(loaded.body.context.active_model,model);assert.equal(count('action_archive'),before+1);}
 }
 assert.equal(serverError,undefined);assert.deepEqual(errors,[]);
 await writeFile(path.join(output,'result.json'),JSON.stringify({status:'PASS',scope:'compiled full app with actual native get_views capture; HTTP record/action fixtures, separate native authorization receipt',host:hostname(),time:new Date().toISOString(),source_revision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),node:process.version,browser:browser.version(),playwright:{directory:qaDir,version:playwrightVersion,mode:(qaInfo.mode & 0o777).toString(8),uid:qaInfo.uid},capture_revision:captureRevision,capture_sha256:createHash('sha256').update(captureBytes).digest('hex'),assets:manifest.assets,checks,screenshots,errors,calls},null,2)+'\n');
}catch(error){
 await writeFile(path.join(output,'failure.json'),JSON.stringify({status:'FAIL',host:hostname(),time:new Date().toISOString(),source_revision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),error:String(error),server_error:serverError===undefined?null:String(serverError),errors,calls},null,2)+'\n');
 if(page){await page.screenshot({path:path.join(output,'failure.png'),fullPage:true});await writeFile(path.join(output,'failure.html'),await page.content());}
 throw error;
}finally{invocationGate?.release();await browser?.close();await new Promise(resolve=>server.close(resolve));}

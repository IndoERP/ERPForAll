/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled actual widget adapters; fixture RPC is not backend authorization/persistence proof.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {createServer} from 'node:http';
import {readFile,writeFile,mkdir} from 'node:fs/promises';
import {createRequire} from 'node:module';
import {hostname} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {auditPage} from '../testing/axe.ts';
const root=fileURLToPath(new URL('../../',import.meta.url));
const output=process.env.FIELD_WIDGET_BROWSER_OUTPUT;
assert.ok(output,'FIELD_WIDGET_BROWSER_OUTPUT is required');
await mkdir(output,{recursive:true,mode:0o700});
const require=createRequire(import.meta.url),{chromium}=require(path.join(root,'web/node_modules/playwright'));
const {build}=require(path.join(root,'web/node_modules/esbuild'));
const client=name=>JSON.stringify(path.join(root,'addons/web/static/src/client',name));
const entry=`
import './addons/web/static/src/client/fields.ts';
import {resolveFieldWidget} from ${client('registry.ts')};
import {MessageServices} from ${client('messages.ts')};
import {RpcClient} from ${client('transport.ts')};
import {CurrencyDisplayCache} from ${client('field-currency.ts')};
const scene=document.querySelector('#scene');let formScene,listBody;let handles=[],services,changes=[],errors=[],state='default';
const locale={lang:'en_GB',tz:'Asia/Jakarta',date_format:'%d/%m/%Y',time_format:'%H:%M:%S'};
const numericLocale={decimal_point:',',thousands_sep:'.',grouping:[3,0]};
const report=error=>errors.push({kind:error?.kind,message:error instanceof Error?error.message:String(error)});
function reset(){for(const h of handles)h.dispose();handles=[];services?.dispose();scene.replaceChildren();formScene=document.createElement('div');formScene.id='form-scene';formScene.className='record-form';const title=document.createElement('h2');title.textContent='Readonly list adapters';const table=document.createElement('table');table.className='list-table';table.innerHTML='<thead><tr><th scope=col>Field</th><th scope=col>Read value</th></tr></thead>';listBody=document.createElement('tbody');table.append(listBody);scene.append(formScene,title,table);changes=[];errors=[];services=new MessageServices(scene,report);}
const response=value=>new Response(JSON.stringify(value),{headers:{'Content-Type':'application/json'}});
const rpc=new RpcClient(async(path,init)=>{
 if(path.endsWith('/res.currency/web_read'))return response([{id:1,symbol:'Rp',position:'before',decimal_places:2}]);
 if(path.endsWith('/web_name_search')){
  if(state==='loading')return new Promise((resolve,reject)=>{init.signal?.addEventListener('abort',()=>reject(new DOMException('Aborted','AbortError')),{once:true});});
  if(state==='error')return new Response(JSON.stringify({name:'erp.exceptions.AccessError',message:'Fixture access refused.'}),{status:403,headers:{'Content-Type':'application/json'}});
  return response(state==='empty'?[]:[{id:4,display_name:'Fixture contact'},{id:5,display_name:'Second contact'}]);
 }
 throw new Error('Unexpected fixture RPC '+path);
},()=>{});
function field(name,kind,widget=kind,options={}){return {name,kind,widget,label:name.replaceAll('_',' '),required:state==='required',readonly:state==='readonly'||state==='disabled',relation:kind==='many2one'||kind==='many2many'?'fixture.contact':null,relationField:null,selection:kind==='selection'?[['draft','Draft'],['ready','Ready'],['done','Done']]:null,options};}
function show(next){reset();state=next;const cache=new CurrencyDisplayCache();
 const data=[['Reference','char','char','ERP-001',{help:'A record reference',placeholder:'Reference'}],['Description','text','text','A compact office field',{placeholder:'Description'}],['Access_phrase','char','password','Synthetic fixture',{placeholder:'Private value'}],['Email','char','email','fixture@example.test',{}],['Website','char','url','https://example.test',{}],['Phone','char','phone','+441234567890',{}],['Quantity','integer','integer',1234,{}],['Ratio','float','float',0.29,{digits:[10,2]}],['Amount','monetary','monetary',12.5,{currency_field:{model:'fixture.record',name:'currency_id'}}],['Active','boolean','boolean',true,{}],['Checkbox','boolean','boolean_checkbox',true,{}],['Favorite','boolean','boolean_favorite',false,{}],['Indicator','boolean','boolean_icon',true,{icon:'●'}],['Stage_placeholder','selection','selection',false,{placeholder:'Fallback',placeholder_field:{model:'fixture.record',name:'placeholder_hint'}}],['Stage','selection','selection','draft',{}],['Stage_badges','selection','badges_selection','ready',{badge_limit:2}],['Stage_badge','selection','badge','ready',{}],['Stage_label','selection','label_selection','ready',{}],['Stage_filtered','selection','filterable_selection','ready',{whitelisted_values:['draft']}],['Order_date','date','date','2026-11-10',{warn:'future'}],['Date_range','date','daterange','2026-11-10',{end_date_field:{model:'fixture.record',name:'Range_end'},always_range:true}],['Appointment','datetime','datetime','2026-11-10 03:15:30',{}],['Contact','many2one','many2one',false,{no_create:true}],['Tags','many2many','many2many_tags',[{id:4,display_name:'Fixture contact'}],{no_create:true}],['Colored_tags','many2many','many2many_tags_color_dot',[{id:4,display_name:'Fixture contact',color:'#12abef'}],{no_create:true,color_field:{model:'fixture.contact',name:'color'}}]];
 for(const [name,kind,widget,represented,options] of data){if(['loading','error'].includes(state)&&name!=='Contact')continue;const f=field(name,kind,widget,options),value=state==='empty'||state==='required'?false:represented;
 const values={placeholder_hint:'Choose a stage',currency_id:{id:1,display_name:'Fixture rupiah'},Date_range:value,Range_end:state==='empty'||state==='required'?false:'2026-11-12'};const fields=[f,field('Range_end','date'),field('placeholder_hint','char')];const h=resolveFieldWidget(widget,'form').render({field:f,value,values,fields,changeValues:values=>changes.push({name,values}),currencies:cache,numericLocale,editorLocale:locale,messages:services,rpc,context:{lang:'en_GB',tz:'Asia/Jakarta'},run:job=>{void job().catch(report);},report,change:value=>changes.push({name,value})});handles.push(h);formScene.append(h.node);const read=resolveFieldWidget(widget,'list').render({field:{...f,readonly:true},value,values,fields:fields.map(field=>({...field,readonly:true})),viewContext:'list',currencies:cache,numericLocale,editorLocale:locale,messages:services,rpc,context:{lang:'en_GB',tz:'Asia/Jakarta'},run:job=>{void job().catch(report);},report,change:()=>{throw new Error('Readonly list mutation');}});handles.push(read);const row=document.createElement('tr'),label=document.createElement('th'),cell=document.createElement('td');label.scope='row';label.textContent=f.label;cell.append(read.node);row.append(label,cell);listBody.append(row);}
 document.body.dataset.ready='true';}
window.fixture={show,state:()=>({changes,errors}),dispose:reset};show('default');
`;
const bundle=await build({stdin:{contents:entry,resolveDir:root,sourcefile:'field-widgets-fixture.ts',loader:'ts'},bundle:true,write:false,format:'esm',platform:'browser',target:'es2023',metafile:true,alias:{'@erpforall/elang':path.join(root,'web/src/index.ts'),'@erpforall/expr':path.join(root,'web/packages/expr/src/index.ts')}});
const javascript=bundle.outputFiles[0].contents,css=await readFile(path.join(root,'addons/web/static/src/client/client.css'));
const hash=data=>createHash('sha256').update(data).digest('hex'),sources=[];
for(const name of Object.keys(bundle.metafile.inputs).sort()){if(name.includes('node_modules')||name==='field-widgets-fixture.ts')continue;const file=path.resolve(root,name);sources.push({path:path.relative(root,file),sha256:hash(await readFile(file))});}
for(const name of ['addons/web/static/src/client/client.css','web/scripts/browser-field-widgets.mjs','web/pnpm-lock.yaml'])sources.push({path:name,sha256:hash(await readFile(path.join(root,name)))});
const server=createServer((request,response)=>{if(request.url==='/fixture.js'){response.writeHead(200,{'Content-Type':'text/javascript'});response.end(javascript);}else if(request.url==='/client.css'){response.writeHead(200,{'Content-Type':'text/css'});response.end(css);}else if(request.url==='/'){response.writeHead(200,{'Content-Type':'text/html'});response.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Field widget acceptance</title><link rel="stylesheet" href="/client.css"></head><body><main style="padding:24px;max-width:1000px;margin:auto"><h1>Field widgets</h1><section id="scene" aria-label="Field widget fixture"></section><button id="after" type="button">Next field</button></main><script type="module" src="/fixture.js"></script></body></html>');}else{response.writeHead(404);response.end();}});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
const result={host:hostname(),utc:new Date().toISOString(),node:process.version,revision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),boundary:'Compiled widget fixture; no backend/auth/persistence claim',sources,checks:[],captures:[],errors:[]};
let browser;
try{
 browser=await chromium.launch({headless:true});result.browser=browser.version();
 const page=await browser.newPage();const form=page.locator('#form-scene');page.setDefaultTimeout(8000);page.on('pageerror',error=>result.errors.push(error.message));
 await page.goto('http://127.0.0.1:'+server.address().port);await page.waitForSelector('body[data-ready="true"]');
 await form.getByRole('button',{name:'Reveal Access phrase',exact:true}).click();assert.equal(await form.locator('[name="Access_phrase"]').getAttribute('type'),'text');assert.deepEqual((await page.evaluate(()=>window.fixture.state())).changes,[]);result.checks.push('password reveal changes presentation only');
 await page.evaluate(()=>window.fixture.show('default'));await form.locator('[name="Quantity"]').fill('2.345');await form.locator('[name="Quantity"]').dispatchEvent('change');assert.deepEqual((await page.evaluate(()=>window.fixture.state())).changes,[{name:'Quantity',value:2345}]);result.checks.push('localized integer commits represented identity');
 await page.evaluate(()=>window.fixture.show('default'));await form.locator('[name="Quantity"]').fill('2.345');await form.locator('[name="Quantity"]').press('Tab');assert.deepEqual((await page.evaluate(()=>window.fixture.state())).changes,[{name:'Quantity',value:2345}]);result.checks.push('Tab commits a localized numeric draft');
 await page.evaluate(()=>window.fixture.show('default'));await form.locator('[name="Quantity"]').fill('invalid');await form.locator('[name="Quantity"]').press('Escape');assert.equal(await form.locator('[name="Quantity"]').inputValue(),'1.234');assert.equal(await form.locator('[name="Quantity"]').evaluate(input=>input.validity.valid),true);assert.deepEqual((await page.evaluate(()=>window.fixture.state())).changes,[]);result.checks.push('Escape restores represented numeric text without mutation');
 await page.evaluate(()=>window.fixture.show('default'));await form.locator('[name="Quantity"]').fill('2.345');await form.locator('[name="Quantity"]').press('Enter');assert.deepEqual((await page.evaluate(()=>window.fixture.state())).changes,[{name:'Quantity',value:2345}]);result.checks.push('Enter commits a localized numeric draft');
 await page.evaluate(()=>window.fixture.show('default'));await page.waitForFunction(()=>!document.querySelector('.field-monetary [aria-busy="true"]'));
 await page.setViewportSize({width:1280,height:1000});
 const amountBox=await form.locator('[name="Amount"]').boundingBox(),symbolBox=await form.locator('.field-currency-symbol').boundingBox();assert.ok(amountBox&&symbolBox);assert.ok(Math.abs((amountBox.y+amountBox.height/2)-(symbolBox.y+symbolBox.height/2))<=2,'currency symbol and amount must share a presentation row');result.checks.push('currency symbol and amount share one presentation row');
 assert.equal(await form.locator('[name="Stage_placeholder"] option[value=""]').textContent(),'Choose a stage');result.checks.push('selection consumes authorized text placeholder dependency');
 for(const theme of ['light','dark'])for(const width of [1280,900,390])for(const state of ['default','hover','focus','active','disabled','readonly','required','invalid','loading','empty','error','dirty']){
  await page.setViewportSize({width,height:1000});await page.evaluate(({theme,state})=>{document.documentElement.dataset.theme=theme;window.fixture.show(state);},{theme,state});
  if(state==='hover')await form.locator('[name="Reference"]').hover();
  if(state==='focus')await form.locator('[name="Reference"]').focus();
  if(state==='active')await form.getByRole('button',{name:'Favorite',exact:true}).click();
  if(state==='invalid'){await form.locator('[name="Quantity"]').fill('invalid');await form.locator('[name="Quantity"]').dispatchEvent('input');assert.equal(await form.locator('[name="Quantity"]').getAttribute('aria-invalid'),'true');}
  if(state==='dirty'){await form.locator('[name="Reference"]').fill('Draft reference');assert.equal(await form.locator('.field-dirty').count(),1);}
  if(['loading','empty','error'].includes(state)){await form.getByRole('combobox',{name:'Search Contact',exact:true}).fill('query');if(state==='loading')await page.waitForSelector('[aria-busy="true"]');else await page.waitForFunction(()=>!document.querySelector('[role="combobox"][aria-busy="true"]'));await page.waitForTimeout(200);}
  else await page.waitForFunction(()=>!document.querySelector('.field-monetary [aria-busy="true"]'));
  const audit=await auditPage(page);assert.equal(audit.violations.length,0,JSON.stringify(audit.violations));assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'horizontal overflow');
  const name=theme+'-'+width+'-'+state;await writeFile(path.join(output,name+'.axe.json'),JSON.stringify(audit,null,2));const png=await page.screenshot({path:path.join(output,name+'.png'),fullPage:true});result.captures.push({name,sha256:hash(png),axe_violations:audit.violations.length});
 }
 assert.deepEqual(result.errors,[]);result.status='passed';
}catch(error){result.status='failed';result.errors.push(error instanceof Error?error.message:String(error));throw error;}
finally{await writeFile(path.join(output,'manifest.json'),JSON.stringify(result,null,2));await browser?.close();await new Promise(resolve=>server.close(resolve));}

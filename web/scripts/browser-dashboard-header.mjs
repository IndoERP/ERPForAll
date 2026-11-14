/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real dashboard component, synthetic records and RPC replies. Backend acceptance is a separate test.
import assert from "node:assert/strict";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { hostname } from "node:os";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { auditPage } from "../testing/axe.ts";
const root=fileURLToPath(new URL("../../",import.meta.url));
const output=process.env.DASHBOARD_OUTPUT;
if(!output)throw new Error("Set DASHBOARD_OUTPUT to a new evidence directory.");
await mkdir(output,{recursive:true});
const entry=`import {mountDashboard} from ${JSON.stringify(path.join(root,"addons/web/static/src/client/dashboard.ts"))};
const selected=new Set();let reads=0;
const config={version:1,revision:'a'.repeat(64),items:[{kind:'count',name:'available',label:'Available books',color:'success'},{kind:'count',name:'unavailable',label:'Unavailable books',color:null},{kind:'kpi',name:'pages',label:'Pages added',color:null}]};
let failure=false;
const dashboard=mountDashboard({config,read:async(scope,period)=>{reads++;if(failure)throw new Error('The request could not be completed.');return {version:1,hidden:false,revision:config.revision,period:period||'week',periods:[{id:'week',label:'Last 7 days',kind:'rolling',days:7},{id:'month',label:'This month',kind:'month',days:null}],values:[{name:'available',value:'28',domain:[['available','=',true]]},{name:'unavailable',value:'4',domain:[['available','=',false]]},{name:'pages',value:'10840',previous:'10000',change:'8.4'}]};},apply:name=>{selected.has(name)?selected.delete(name):selected.add(name);document.querySelector('#facet').textContent=selected.size?'Available books':'';void dashboard.refresh([['available','=',true]]);},selected:name=>selected.has(name)});
document.querySelector('main').append(dashboard.element);
const table=document.createElement('table');table.className='list-table';
for(const [index,row] of [['Title','Author','Pages','Availability'],['The left hand of darkness','Ursula Le Guin','304','Available'],['Kindred','Octavia Butler','264','Available'],['Solaris','Stanisław Lem','204','On loan']].entries()){const tr=document.createElement('tr');for(const value of row){const td=document.createElement(index===0?'th':'td');if(index===0)td.scope='col';td.textContent=value;tr.append(td);}table.append(tr);}document.querySelector('main').append(table);
window.fixture={dashboard,reads:()=>reads,fail:async()=>{failure=true;await dashboard.refresh([],true);},recover:async()=>{failure=false;await dashboard.refresh([],true);}};
await dashboard.refresh([]);window.fixtureReady=true;`;
const bundle=await build({stdin:{contents:entry,resolveDir:path.join(root,"web"),sourcefile:"dashboard-fixture.ts",loader:"ts"},bundle:true,write:false,format:"esm",platform:"browser",target:"es2023"});
const css=await readFile(path.join(root,"addons/web/static/src/client/client.css"));
const script=bundle.outputFiles[0].contents;
const server=createServer((request,response)=>{
 if(request.url==="/fixture.js"){response.setHeader("Content-Type","text/javascript");response.end(script);return;}
 if(request.url==="/client.css"){response.setHeader("Content-Type","text/css");response.end(css);return;}
 const theme=new URL(request.url,"http://127.0.0.1").searchParams.get("theme");
 if(theme!=="light"&&theme!=="dark"){response.writeHead(400);response.end();return;}
 response.setHeader("Content-Type","text/html; charset=utf-8");
 response.end('<!doctype html><html lang="en" data-theme="'+theme+'"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Library dashboard fixture</title><link rel="stylesheet" href="/client.css"><style>body{margin:0}main{max-width:1440px;margin:auto}.fixture-header{padding:16px;display:flex;align-items:center;justify-content:space-between}.fixture-header h1{font-size:20px;margin:0}table{width:100%;border-collapse:collapse}td,th{padding:8px 12px;border-bottom:1px solid var(--line)}th{color:var(--text);background:var(--surface-subtle);text-align:left}#facet{padding:4px 8px;background:var(--selected);border-radius:var(--radius-control)}@media(max-width:700px){td:nth-child(2){display:none}}</style></head><body><main><div class="fixture-header"><h1>Books</h1><span id="facet"></span><button>New</button></div></main><script type="module" src="/fixture.js"></script></body></html>');
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const require=createRequire(path.join(process.env.HOME,".local/share/erpforall-qa/package.json"));
const {chromium}=require("playwright");
let browser;const errors=[];const captures=[];
try{
 browser=await chromium.launch({headless:true,...(process.env.CHROME_BIN?{executablePath:process.env.CHROME_BIN}:{})});
 const page=await browser.newPage();page.on("pageerror",error=>errors.push(error.message));
 for(const theme of ["light","dark"]){
  for(const width of [1440,1280,768,390]){
   await page.setViewportSize({width,height:900});await page.emulateMedia({colorScheme:theme,reducedMotion:"reduce"});await page.goto(`http://127.0.0.1:${server.address().port}/?theme=${theme}`);await page.waitForFunction(()=>window.fixtureReady===true);

   const metrics=await page.locator('.dashboard-strip').evaluate(element=>({width:element.getBoundingClientRect().width,busy:element.getAttribute('aria-busy'),scroll:document.documentElement.scrollWidth,viewport:innerWidth}));
   assert.equal(metrics.busy,'false');assert.ok(metrics.scroll<=width,JSON.stringify(metrics));
   const card=page.locator('button[data-dashboard-item="available"]');await card.focus();await page.keyboard.press('Enter');await page.waitForFunction(()=>document.querySelector('[data-dashboard-item="available"]').getAttribute('aria-pressed')==='true');
   assert.equal(await page.locator('#facet').textContent(),'Available books');
   await page.locator('select[aria-label="Dashboard period"]').selectOption('month');await page.waitForFunction(()=>document.querySelector('select').value==='month'&&document.querySelector('.dashboard-strip').getAttribute('aria-busy')==='false');
   await auditPage(page);
   const file=`dashboard-${theme}-${width}.png`;await page.screenshot({path:path.join(output,file),fullPage:true});captures.push({file,theme,width,metrics});
  }
 }
 await page.evaluate(()=>window.fixture.fail());assert.match(await page.locator('[role="alert"]').textContent(),/Dashboard unavailable/);await page.screenshot({path:path.join(output,'dashboard-error-390.png'),fullPage:true});
 await page.evaluate(()=>window.fixture.recover());assert.equal(await page.locator('.dashboard-value').first().textContent(),'28');
 assert.deepEqual(errors,[]);
 const result={host:hostname(),at:new Date().toISOString(),node:process.version,browser:browser.version(),command:process.argv,sourceRevision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),captures,errors,scope:'Compiled component fixture with synthetic responses; no live server or backend parity claim.'};
 await writeFile(path.join(output,'result.json'),JSON.stringify(result,null,2)+'\n');console.log(JSON.stringify(result));
}finally{await browser?.close();await new Promise(resolve=>server.close(resolve));}

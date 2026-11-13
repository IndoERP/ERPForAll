/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual compiled client assets with explicit HTTP fixtures. No backend/auth/database parity claim.
import assert from "node:assert/strict";
import { readFile, mkdir, writeFile, stat, realpath } from "node:fs/promises";
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { hostname, homedir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import path from "node:path";
// refuse ancestor resolution and installations outside the private operator directory.
const qaRoot = path.join(homedir(), ".local/share/erpforall-qa");
const qaInfo = await stat(qaRoot);
assert.ok(qaInfo.isDirectory() && qaInfo.uid === process.getuid() && (qaInfo.mode & 0o777) === 0o700, "Playwright tooling directory must be operator-owned mode 700.");
const playwrightPath = await realpath(path.join(qaRoot, "node_modules/playwright/index.mjs"));
assert.ok(playwrightPath.startsWith((await realpath(qaRoot)) + path.sep), "Playwright must resolve inside the operator-owned QA directory.");
const { chromium } = await import(pathToFileURL(playwrightPath).href);
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.CLIENT_BROWSER_OUTPUT;
const theme = process.env.CLIENT_BROWSER_THEME ?? "light";
assert.ok(["light", "dark"].includes(theme), "CLIENT_BROWSER_THEME must be light or dark");
if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to an isolated output directory.");
await mkdir(output, {recursive:true});
const manifest = JSON.parse(await readFile(path.join(root,"web/dist/human-ui/manifest.json"),"utf8"));
const assets = new Map();
for (const asset of manifest.assets) {
  const data = await readFile(path.join(root,"web/dist/human-ui",asset.path));
  assert.equal(createHash("sha256").update(data).digest("hex"),asset.sha256);
  assets.set("/erp/assets/"+asset.path,{data,type:asset.content_type});
}
const js = manifest.assets.find(a=>a.name==="client.js"), css=manifest.assets.find(a=>a.name==="client.css");
const bootstrap={wire_major:1,registry_version:"r1",asset_version:"fixture-a1",identity:{uid:2,name:"Fixture user",internal:true},companies:[{id:1,name:"Fixture company"}],current_company_id:1,allowed_company_ids:[1],active_company_ids:[1],locale:{lang:"en_US",tz:"UTC"},csrf:{logout:"fixture-not-secret"}};
const field=(type,label,required=false,extra={})=>({type,label,required,readonly:false,selection:null,relation:null,relation_field:null,...extra});
const modifiers={readonly:null,required:null,invisible:null};
const fieldNode=(model,name,type="char")=>({kind:"field", force_save: false,field:{model,name},widget:type,options:{},label:name,modifiers});
const objectButton=(name,label,confirm=null)=>({kind:"button",name,label,button_type:"object",variant:"primary",confirm,modifiers});
function description(model,fields,tree){
  const nodes=[];
  function add(node){const index=nodes.length;nodes.push({});const {children,...props}=node;nodes[index]={...props,...(children?{children:children.map(add)}:{})};return index;}
  add(tree);
  return {wire_major:1,registry_version:"r1",description_version:"d1",model,fields:{[model]:fields},notices:[],views:[{identity:{kind:"stored",id:1},type:"form",schema_major:1,renderer:"web.form",root:0,nodes}]};
}
const book=description("test.book",{name:field("char","Book",true)},{kind:"form",record_mode:"record",children:[{kind:"header",children:[objectButton("lend","Lend book")]},fieldNode("test.book","name")]});
const wizard=description("test.wizard",{name:field("char","Borrower",true),quantity:field("integer","Quantity"),due:field("date","Due"),active:field("boolean","Active"),reason:field("text","Reason",true),tags:field("many2many","Tags",false,{relation:"test.tag"})},
  {kind:"form",record_mode:"record",children:[{kind:"sheet",children:[fieldNode("test.wizard","name"),{kind:"group",label:"Loan",columns:4,colspan:2,children:[fieldNode("test.wizard","quantity","integer"),fieldNode("test.wizard","due","date"),fieldNode("test.wizard","active","boolean")]},{kind:"notebook",children:[{kind:"page",name:"details",label:"Details",children:[fieldNode("test.wizard","tags","many2many")]},{kind:"page",name:"reason",label:"Reason",children:[fieldNode("test.wizard","reason","text")]}]}]},{kind:"footer",children:[objectButton("confirm_loan","Confirm loan","Confirm this loan?"),{kind:"cancel_button",label:"Cancel wizard",variant:"secondary",modifiers}]}]});
for (const name of ["logo.svg", "logo-dark.svg"]) {
  assets.set("/erp/brand/"+name,{data:await readFile(path.join(root,"crates/erp-server/brand/erpforall-"+name)),type:"image/svg+xml"});
}
const calls=[], errors=[], checks=[], screenshots=[];
let persisted={id:77,name:"Ada",quantity:1,due:"2026-11-01",active:true,reason:"",tags:[]};
let serverError, mode="success";
const server=createServer(async(req,res)=>{
 try {
  const pathname=new URL(req.url,"http://fixture").pathname;
  const asset=assets.get(pathname);
  if(asset){res.setHeader("Content-Type",asset.type);res.end(asset.data);return;}
  if(req.method==="GET" && ["/erp","/erp/action-20"].includes(pathname)){
   res.setHeader("Content-Type","text/html");res.end(`<!doctype html><html lang="en" data-theme="${theme}"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Wizard fixture</title><div id="erp-root"></div><script type="application/json" id="erp-bootstrap">${JSON.stringify(bootstrap)}</script><script type="module" src="/erp/assets/${js.path}"></script></html>`);return;
  }
  if(pathname==="/favicon.ico"){res.writeHead(204);res.end();return;}
  let input="";for await(const chunk of req){input+=chunk;if(input.length>32768)throw new Error("Oversized fixture request");}
  const body=JSON.parse(input);calls.push({path:pathname,body});let value;
  if(pathname.endsWith("confirm_loan") && mode==="permission") {res.writeHead(403,{"Content-Type":"application/json"});res.end(JSON.stringify({name:"erp.exceptions.AccessError",message:"Loan denied by fixture."}));return;}
  if(pathname.endsWith("confirm_loan") && mode==="uncertain") {res.destroy();return;}
  if(pathname.endsWith("erp_normalize_action_result") && !body.result?.open && mode==="normalization") {res.writeHead(422,{"Content-Type":"application/json"});res.end(JSON.stringify({name:"erp.exceptions.ValidationError",message:"Result refused by fixture."}));return;}
  if(pathname.includes("test.wizard/web_read") && mode==="expired") {res.writeHead(401,{"Content-Type":"application/json"});res.end("{}");return;}
  if(pathname.endsWith("erp_load_web_menus"))value={wire_major:1,registry_version:"r1",roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:"Books",action_id:20,children:[],icon:null}]};
  else if((pathname.endsWith("erp_load_web_action") || pathname.endsWith("erp_load_web_action_views")))value={wire_major:1,id:20,name:"Books",model:"test.book",target:"current",views:[{id:1,type:"form"}],domain:null,context:null};
  else if(pathname.endsWith("get_views"))value=pathname.includes("test.wizard")?wizard:book;
  else if(pathname.endsWith("erp_normalize_action_result"))value={wire_major:1,result:body.result?.open?{kind:"window",name:"Lend book",model:"test.wizard",res_id:null,views:[{id:null,type:"form"}],context:{default_name:"Ada",default_tags:[[6,0,[]]]}}:{kind:"notification",title:"Loan",message:"<b>Saved</b>",level:"success",then_close:mode!=="stay"}};
  else if(pathname.endsWith("/lend"))value={open:true};
  else if(pathname.endsWith("default_get"))value={name:"Ada",quantity:1,due:"2026-11-01",active:true,reason:"",tags:[[6,0,[]]]};
  else if(pathname.endsWith("onchange"))value={value:{}};
  else if(pathname.endsWith("web_save")){persisted={...persisted,...body.vals,tags:[]};value=[persisted];}
  else if(pathname.endsWith("confirm_loan"))value=true;
  else if(pathname.endsWith("web_read"))value=pathname.includes("test.book")?[{id:5,name:"Dune"}]:[persisted];
  else if(pathname.endsWith("name_search"))value=[];
  else throw new Error(`Unknown fixture RPC: ${pathname}`);
  res.setHeader("Content-Type","application/json");res.end(JSON.stringify(pathname.endsWith("erp_load_web_action_views") && res.statusCode === 200 ? { action: value, views: book } : value));
 }catch(error){serverError=error;res.writeHead(500,{"Content-Type":"application/json"});res.end(JSON.stringify({message:"Fixture server failure"}));}
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({executablePath:process.env.CHROME_BIN??"/usr/bin/google-chrome",headless:true});
const result={status:"FAIL",scope:"compiled client assets with explicit HTTP fixtures; not backend/auth/database acceptance",host:hostname(),time_utc:new Date().toISOString(),node:process.version,theme, source_revision:(await readFile(path.join(root,"REVISION"),"utf8")).trim(),browser:browser.version(),assets:manifest.assets,checks,screenshots};
let page;
try{
 page=await browser.newPage();page.on("pageerror",error=>errors.push(error.message));
 for(const width of [1440,820,390]){
  await page.setViewportSize({width,height:900});
  await page.goto(origin+"/erp/action-20?id=5&view_type=form");
  await page.getByRole("button",{name:"Lend book",exact:true}).click();
  const dialog=page.locator(".wizard-dialog");await dialog.waitFor({state:"visible"});
  assert.equal(await page.locator(".wizard-dialog").count(),1);
  assert.equal(new URL(page.url()).search,"?id=5&view_type=form");
  assert.equal(await dialog.locator('input[name="name"]').inputValue(),"Ada");
  assert.equal(await page.locator(".wizard-footer button").count(),2);
  assert.equal(await page.locator(".wizard-dialog .content-sheet .wizard-footer").count(),0);
  assert.equal(await page.evaluate(()=>document.querySelector('.wizard-dialog').contains(document.activeElement)),true);
  // Native modal focus cannot move to the background launcher even by script.
  assert.equal(await page.evaluate(()=>{document.querySelector('[data-client-content] .form-header-btn').focus();return document.querySelector('.wizard-dialog').contains(document.activeElement);}),true);
  for(let i=0;i<16;i++){await page.keyboard.press("Tab");assert.equal(await page.evaluate(()=>document.activeElement===document.body||document.querySelector('.wizard-dialog').contains(document.activeElement)),true);}
  await dialog.getByRole("button",{name:"Confirm loan",exact:true}).click();
  await page.locator(".choice-dialog").getByRole("button",{name:"Confirm loan",exact:true}).click();
  await dialog.locator('[role="tab"][aria-selected="true"]').filter({hasText:"Reason"}).waitFor();
  assert.equal(await dialog.locator('textarea[name="reason"]').evaluate(node=>node===document.activeElement),true);
  assert.equal(calls.filter(c=>c.path.endsWith("confirm_loan")).length,checks.filter(c=>c.confirmed).length);
  await dialog.locator('textarea[name="reason"]').fill("Read for research");await dialog.locator('textarea[name="reason"]').press("Tab");
  await dialog.getByRole("button",{name:"Confirm loan",exact:true}).waitFor({state:"visible"});
  const geometry=await dialog.evaluate(node=>({width:node.getBoundingClientRect().width,viewport:innerWidth,scroll:node.scrollWidth,client:node.clientWidth,buttons:[...node.querySelectorAll('.wizard-footer button')].map(b=>({width:b.getBoundingClientRect().width,height:b.getBoundingClientRect().height}))}));
  const surfaces = await dialog.evaluate(node=>{
    const background=n=>{for(let p=n;p;p=p.parentElement){const bg=getComputedStyle(p).backgroundColor;if(bg!=="rgba(0, 0, 0, 0)"&&bg!=="transparent")return bg;}throw new Error("Missing rendered background");};
    return [node,...node.querySelectorAll('input:not([type=checkbox]):not([type=hidden]),select,textarea')].filter(n=>n.getBoundingClientRect().width&&n.getBoundingClientRect().height).map(n=>({element:n.tagName,background:background(n),color:getComputedStyle(n).color}));
  });
  const rgb=value=>value.match(/[\d.]+/g).slice(0,3).map(Number);
  const luminance=value=>rgb(value).map(c=>{const v=c/255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4}).reduce((sum,n,i)=>sum+n*[.2126,.7152,.0722][i],0);
  for(const colors of surfaces){
    const bg=luminance(colors.background),fg=luminance(colors.color);
    assert.ok((Math.max(bg,fg)+.05)/(Math.min(bg,fg)+.05)>=4.5,`wizard text is readable: ${JSON.stringify(colors)}`);
    if(theme==="dark")assert.ok(bg<.18,"wizard follows the dark document theme");
    else assert.ok(bg>.7,"wizard follows the light document theme");
  }
  const check={width,geometry,surfaces,confirmed:false};checks.push(check);
  const shot=`wizard-${width}.png`;await page.screenshot({path:path.join(output,shot)});screenshots.push(shot);
  assert.ok(geometry.width<=width);assert.ok(geometry.scroll<=geometry.client+1);assert.ok(geometry.buttons.every(b=>b.height>=40&&b.width>=40));
  const before=calls.filter(c=>c.path.includes("test.book/web_read")).length;
  await dialog.getByRole("button",{name:"Confirm loan",exact:true}).click();
  await page.locator(".choice-dialog").getByRole("button",{name:"Confirm loan",exact:true}).click();
  await dialog.waitFor({state:"detached"});
  await page.waitForFunction(()=>document.querySelector('[data-client-notice]')?.textContent==='Loan: <b>Saved</b>');
  assert.equal(calls.filter(c=>c.path.includes("test.book/web_read")).length,before+1);
  assert.equal(await page.locator('[data-client-notice] b').count(),0);
  assert.equal(await page.getByRole("button",{name:"Lend book",exact:true}).evaluate(node=>node===document.activeElement),true);
  Object.assign(check,{confirmed:true,modal_focus:true,invalid_tab_revealed:true,caller_refresh_once:true});
 }
 await page.setViewportSize({width:1440,height:900});
 const open=async()=>{await page.getByRole("button",{name:"Lend book",exact:true}).click();await page.locator(".wizard-dialog").waitFor({state:"visible"});};
 const ready=async()=>{const d=page.locator(".wizard-dialog");await d.getByRole("tab",{name:"Reason",exact:true}).click();await d.locator('textarea[name="reason"]').fill("Retained reason");await d.locator('textarea[name="reason"]').press("Tab");return d;};
 const confirm=async d=>{await d.getByRole("button",{name:"Confirm loan",exact:true}).click();await page.locator(".choice-dialog").getByRole("button",{name:"Confirm loan",exact:true}).click();};
 mode="stay";await open();let live=await ready();await confirm(live);
 await page.waitForFunction(()=>document.querySelector('.wizard-dialog [data-wizard-notice]')?.textContent==='Loan: <b>Saved</b>');
 assert.equal(await live.locator('[data-wizard-notice]').getAttribute("role"),"status");
 assert.equal(await live.locator('[data-wizard-notice] b').count(),0);
 assert.equal(await live.isVisible(),true);
 await live.getByRole("button",{name:"Cancel wizard",exact:true}).click();await live.waitFor({state:"detached"});
 checks.push({scenario:"nonclosing text notification is in active modal",passed:true});
 mode="success";
 let before=calls.length;
 await open();await page.keyboard.press("Escape");
 await page.locator(".wizard-dialog").waitFor({state:"detached"});
 assert.equal(await page.locator(".choice-dialog").count(),0);
 assert.ok(!calls.slice(before).some(c=>c.path.includes("test.wizard/web_save")||c.path.endsWith("confirm_loan")));
 before=calls.length;
 await open();
 await page.locator(".wizard-dialog").getByRole("button",{name:"Cancel wizard",exact:true}).click();
 await page.locator(".wizard-dialog").waitFor({state:"detached"});
 assert.ok(!calls.slice(before).some(c=>c.path.includes("test.wizard/web_save")||c.path.endsWith("confirm_loan")));
 checks.push({scenario:"escape and explicit cancel close without asking or saving",passed:true});
 mode="permission";await open();let d=await ready();await confirm(d);
 await page.locator(".client-error-dialog").waitFor({state:"visible"});
 assert.equal(await page.locator("#client-error-message").textContent(),"Loan denied by fixture.");
 await page.locator(".client-error-dialog").getByRole("button",{name:"Close",exact:true}).click();
 assert.equal(await d.locator('textarea[name="reason"]').inputValue(),"Retained reason");
 await d.getByRole("button",{name:"Cancel wizard",exact:true}).click();await d.waitFor({state:"detached"});
 checks.push({scenario:"permission error retains edits and can cancel",passed:true});
 mode="normalization";await open();d=await ready();await confirm(d);
 await page.locator(".client-error-dialog").waitFor({state:"visible"});
 assert.equal(await page.locator("#client-error-message").textContent(),"Result refused by fixture.");
 await page.locator(".client-error-dialog").getByRole("button",{name:"Close",exact:true}).click();
 assert.equal(await d.getByRole("button",{name:"Confirm loan",exact:true}).isDisabled(),true);
 checks.push({scenario:"normalization failure locks method",passed:true});
 mode="success";await page.reload();await open();d=await ready();
 before=calls.filter(c=>c.path.endsWith("confirm_loan")).length;
 await page.waitForFunction(()=>[...document.querySelectorAll('.wizard-footer button')].some(node=>node.textContent==='Confirm loan'&&!node.disabled));
 await d.getByRole("button",{name:"Confirm loan",exact:true}).evaluate(node=>{node.click();node.click();});
 await page.locator(".choice-dialog").getByRole("button",{name:"Confirm loan",exact:true}).evaluate(node=>{node.click();node.click();});
 await d.waitFor({state:"detached"});assert.equal(calls.filter(c=>c.path.endsWith("confirm_loan")).length,before+1);
 checks.push({scenario:"duplicate activations execute one method",passed:true});
 mode="uncertain";await open();d=await ready();await confirm(d);
 await page.locator(".client-error-dialog").waitFor({state:"visible"});
 assert.equal(await page.locator("#client-error-message").textContent(),"The connection was interrupted. The change may have been saved. Check the record before trying again.");
 await page.locator(".client-error-dialog").getByRole("button",{name:"Close",exact:true}).click();
 await d.getByRole("button",{name:"Reload wizard",exact:true}).click();
 await page.waitForFunction(()=>!document.querySelector('.wizard-reload').disabled);
 assert.equal(await d.getByRole("button",{name:"Confirm loan",exact:true}).isDisabled(),true);
 checks.push({scenario:"uncertain method stays locked after readback",passed:true});
 mode="expired";await d.getByRole("button",{name:"Reload wizard",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#erp-root').textContent.includes('Your session has expired.'));
 assert.equal(await page.locator("dialog").count(),0);
 checks.push({scenario:"session expiry disposes protected modal",passed:true});
 if(serverError)throw serverError;assert.deepEqual(errors,[]);result.status="PASS";
}catch(error){result.failure=String(error.stack??error); if(page){result.page_text=await page.locator("body").innerText();await page.screenshot({path:path.join(output,"failure.png")});}throw error;}
finally{result.calls=calls;result.errors=errors;await writeFile(path.join(output,"result.json"),JSON.stringify(result,null,2)+"\n");await browser.close();await new Promise(resolve=>server.close(resolve));}

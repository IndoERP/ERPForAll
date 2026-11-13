/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled-asset protocol fixtures only; authenticated database authorization has separate tests.
import assert from "node:assert/strict";
import {readFile,mkdir,writeFile,stat,realpath} from "node:fs/promises";
import {createServer} from "node:http";
import {createHash} from "node:crypto";
import {hostname,homedir} from "node:os";
import {fileURLToPath,pathToFileURL} from "node:url";
import path from "node:path";

const officeMenuProbe=process.env.CLIENT_BROWSER_OFFICE_MENU_PROBE==="1";
const qaRoot=path.join(homedir(),".local/share/erpforall-qa");
const qaInfo=await stat(qaRoot);
assert.ok(qaInfo.isDirectory() && qaInfo.uid===process.getuid() && (qaInfo.mode&0o777)===0o700,"operator-owned mode-700 QA tooling required");
const qaPath=await realpath(path.join(qaRoot,"node_modules/playwright/index.mjs"));
assert.ok(qaPath.startsWith((await realpath(qaRoot))+path.sep));
const {chromium}=await import(pathToFileURL(qaPath).href);
const root=fileURLToPath(new URL("../../",import.meta.url));
const output=process.env.CLIENT_BROWSER_OUTPUT;
assert.ok(output,"CLIENT_BROWSER_OUTPUT required");
await mkdir(output,{recursive:true});
const manifest=JSON.parse(await readFile(path.join(root,"web/dist/human-ui/manifest.json"),"utf8"));
const assets=new Map();
for(const asset of manifest.assets){
  const data=await readFile(path.join(root,"web/dist/human-ui",asset.path));
  assert.equal(createHash("sha256").update(data).digest("hex"),asset.sha256);
  assets.set("/erp/assets/"+asset.path,{data,type:asset.content_type});
}
for(const name of ["logo.svg","logo-dark.svg"]) assets.set("/erp/brand/"+name,{data:await readFile(path.join(root,"crates/erp-server/brand/erpforall-"+name)),type:"image/svg+xml"});
if(officeMenuProbe)assets.set("/theme_office_blue/static/src/theme.css",{data:await readFile(path.join(root,"addons/theme_office_blue/static/src/theme.css")),type:"text/css"});
const js=manifest.assets.find(asset=>asset.name==="client.js"),css=manifest.assets.find(asset=>asset.name==="client.css");
const bootstrap={wire_major:1,registry_version:"r1",asset_version:"fixture-a1",identity:{uid:2,name:"Fixture user",internal:true},companies:[{id:1,name:"Fixture company"}],current_company_id:1,allowed_company_ids:[1],active_company_ids:[1],locale:{lang:"en_US",tz:"UTC"},csrf:{logout:"fixture-not-secret"}};
const modifiers={readonly:null,required:null,invisible:null};
const field={type:"char",label:"Name",required:true,readonly:false,selection:null,relation:null,relation_field:null};
const fieldNode=model=>({kind:"field",force_save:false,field:{model,name:"name"},widget:"char",options:{},label:"Name",modifiers});
const view=(model,type,id,button=false)=>({identity:{kind:"stored",id},type,schema_major:1,renderer:"web."+type,root:0,nodes:button
  ?[{kind:"form",record_mode:"record",children:[1,3]},{kind:"header",children:[2]},{kind:"button",name:"open_related",label:"Related records",button_type:"object",variant:"primary",confirm:null,modifiers},fieldNode(model)]
  :[{kind:type,...(type==="form"?{record_mode:"record"}:{}),children:[1]},fieldNode(model)]});
const description=(model,views)=>({wire_major:1,registry_version:"r1",description_version:model,model,fields:{[model]:{name:field}},notices:[],views});
const source=description("fixture.parent",[view("fixture.parent","form",1,true)]);
const target=description("fixture.line",[view("fixture.line","list",2),view("fixture.line","form",3)]);
const baseDomain=[["parent_id","=",5]];
const current={kind:"current_window",name:"Related fixture records",model:"fixture.line",views:[{id:null,type:"list"},{id:null,type:"form"}],domain:baseDomain,context:{default_name:"Default fixture line"}};
const action={wire_major:1,id:20,name:"Parents",model:"fixture.parent",target:"current",views:[{id:1,type:"form"}],domain:null,context:null};
const calls=[],errors=[],checks=[],geometryDiagnostics=[],screenshots=[];
let serverError,denyTarget=false;
let parentName="Parent fixture";
const server=createServer(async(req,res)=>{
  try{
    const pathname=new URL(req.url,"http://fixture").pathname;
    const asset=assets.get(pathname);
    if(asset){res.setHeader("Content-Type",asset.type);res.end(asset.data);return;}
    if(req.method==="GET" && ["/erp","/erp/action-20"].includes(pathname)){
      res.setHeader("Content-Type","text/html");
      res.end(`<!doctype html><html lang="en"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Current action fixture</title><link rel="stylesheet" href="/erp/assets/${css.path}"><div id="erp-root"></div><script type="application/json" id="erp-bootstrap">${JSON.stringify(bootstrap)}</script><script type="module" src="/erp/assets/${js.path}"></script></html>`);return;
    }
    if(pathname==="/favicon.ico"){res.writeHead(204);res.end();return;}
    let input="";for await(const chunk of req){input+=chunk;if(input.length>32768)throw new Error("oversized fixture request");}
    const body=JSON.parse(input);calls.push({path:pathname,body});let value;
    if(pathname.includes("fixture.line/get_views") && denyTarget){res.writeHead(403,{"Content-Type":"application/json"});res.end(JSON.stringify({name:"erp.exceptions.AccessError",message:"Related destination denied by fixture."}));return;}
    if(pathname.endsWith("ir.config.setting/erp_describe"))value={wire_major:1,registry_version:"r1",scope:{kind:"global"},declarations:officeMenuProbe?[{key:"theme_office_blue.appearance",kind:"selection",label:"Appearance",choices:[["default","Default"],["office-blue","Office Blue"]],writable:true}]:[]};
    else if(pathname.endsWith("erp_read_values"))value={wire_major:1,scope:body.scope,revision:"1",values:[{key:"theme_office_blue.appearance",value:"office-blue",source:"stored"}]};
    else if(pathname.endsWith("erp_load_web_menus"))value={wire_major:1,registry_version:"r1",roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:"Parents",action_id:20,children:[],icon:null}]};
    else if(pathname.endsWith("erp_load_web_action_views"))value={action,views:source};
    else if(pathname.endsWith("erp_load_web_action"))value=action;
    else if(pathname.endsWith("get_views"))value=pathname.includes("fixture.line")?target:source;
    else if(pathname.endsWith("/open_related"))value={related:true};
    else if(pathname.endsWith("erp_normalize_action_result"))value={wire_major:1,result:current};
    else if(pathname.endsWith("web_search_read"))value={length:41,records:[{id:Number(body.offset)+31,name:"Visible fixture line"}]};
    else if(pathname.endsWith("web_save")){parentName=body.vals.name??parentName;value=[{id:5,name:parentName}];}
    else if(pathname.endsWith("web_read"))value=[{id:body.ids[0],name:pathname.includes("fixture.line")?"Visible fixture line":parentName}];
    else if(pathname.endsWith("default_get"))value={name:"Default fixture line"};
    else if(pathname.endsWith("onchange"))value={value:{}};
    else if(pathname.endsWith("web_name_search"))value=[];
    else if(pathname==="/erp/client-error"){res.writeHead(204);res.end();return;}
    else throw new Error(`unknown fixture RPC ${pathname}`);
    if(officeMenuProbe&&pathname.endsWith("erp_load_web_menus")){value={wire_major:1,registry_version:"r1",roots:[11],nodes:[{ module: null,id:11,parent_id:null,label:"Settings",action_id:null,children:[12],icon:null},{ module: null,id:12,parent_id:11,label:"Technical",action_id:null,children:[13],icon:null},{ module: null,id:13,parent_id:12,label:"Database Structure",action_id:null,children:[10],icon:null},{ module: null,id:10,parent_id:13,label:"Sequences and Identifiers",action_id:20,children:[],icon:null}]};}
    res.setHeader("Content-Type","application/json");res.end(JSON.stringify(value));
  }catch(error){serverError=error;res.writeHead(500,{"Content-Type":"application/json"});res.end(JSON.stringify({message:"fixture failure"}));}
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const endpoint=process.env.MOLI_CDP_URL;
assert.ok(endpoint && /^http:\/\/127\.0\.0\.1:\d+$/.test(endpoint),"loopback MOLI_CDP_URL required");
const discovery=await (await fetch(endpoint+"/json/version")).json();
assert.match(String(discovery["Moli-Version"]),/^\d+\.\d+\.\d+/,"the final browser must expose Moli-Version");
const browser=await chromium.connectOverCDP(endpoint);
const context=browser.contexts()[0];assert.ok(context,"Moli default context required");
const result={status:"FAIL",scope:"compiled client assets with explicit protocol fixtures in Moli; not backend/auth/database acceptance",hostname:hostname(),time_utc:new Date().toISOString(),node:process.version,engine:"Moli",browser:browser.version(),browser_discovery:discovery,harness_sha256:createHash("sha256").update(await readFile(fileURLToPath(import.meta.url))).digest("hex"),source_revision:(await readFile(path.join(root,"REVISION"),"utf8")).trim(),assets:manifest.assets,checks,geometryDiagnostics,screenshots};
const page=await context.newPage();page.on("pageerror",error=>errors.push(error.message));
const cdp=await context.newCDPSession(page),worlds=new Map();
cdp.on("Runtime.executionContextCreated",({context})=>worlds.set(context.id,context));
cdp.on("Runtime.executionContextDestroyed",({executionContextId})=>worlds.delete(executionContextId));
cdp.on("Runtime.executionContextsCleared",()=>worlds.clear());
await cdp.send("Runtime.enable");
const geometryExpression=`(()=>{const n=document.querySelector('.form-header-btn');if(!n)return {missing:true};const r=n.getBoundingClientRect(),s=getComputedStyle(n);return {connected:n.isConnected,rect:{x:r.x,y:r.y,width:r.width,height:r.height},rects:Array.from(n.getClientRects()).map(v=>({x:v.x,y:v.y,width:v.width,height:v.height})),disabled:n.disabled,display:s.display,visibility:s.visibility,opacity:s.opacity,checkVisibility:typeof n.checkVisibility==='function'?n.checkVisibility():'unsupported',playwrightVisibility:typeof n.checkVisibility==='function'?n.checkVisibility({contentVisibilityAuto:true,opacityProperty:false,visibilityProperty:false}):'unsupported',hit:document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)?.outerHTML.slice(0,300)}})()`;
const diagnose=async label=>{
  const frame=await cdp.send("Page.getFrameTree");
  await cdp.send("Page.createIsolatedWorld",{frameId:frame.frameTree.frame.id,worldName:"erpforall-current-action-diagnostic"});
  const values=[];
  for(const world of worlds.values()){
    try{values.push({world,result:await cdp.send("Runtime.evaluate",{contextId:world.id,expression:geometryExpression,returnByValue:true})});}
    catch(error){values.push({world,error:String(error)});}
  }
  geometryDiagnostics.push({label,values});
  await writeFile(path.join(output,"geometry.json"),JSON.stringify(geometryDiagnostics,null,2));
};
const rendered=async(locator,label)=>{
  await locator.waitFor({state:"attached"});
  // Moli 1.1.20's DOM geometry can remain stale after a render. Request a real layout/raster
  // before interaction; retain normal Playwright visible/enabled/native input checks.
  const filename=`${label}-${page.viewportSize().width}.png`;
  await page.screenshot({path:path.join(output,filename)});
  screenshots.push(filename);
  await locator.waitFor({state:"visible"});
};
try{
  for(const width of [1440,820,390]){
    await page.setViewportSize({width,height:900});
    await page.goto(origin+"/erp/action-20?id=5&view_type=form");
    await rendered(page.locator('input[name="name"]'),"source");
    await page.locator('input[name="name"]').fill("Edited fixture parent");
    await page.locator('input[name="name"]').press("Tab");
    if(process.env.MOLI_DIAGNOSTIC)await diagnose("before native locator click");
    await rendered(page.getByRole("button",{name:"Related records",exact:true}),"source-dirty");
    if(process.env.MOLI_DIAGNOSTIC)await diagnose("after real render before native locator click");
    const before=calls.filter(call=>call.path.endsWith("/open_related")).length;
    await page.getByRole("button",{name:"Related records",exact:true}).click();
    await rendered(page.locator(".list-table"),"related-list");
    const identity=new URL(page.url()).searchParams.get("method_action");assert.ok(identity);
    assert.ok(!page.url().includes("parent_id"));
    assert.equal(calls.filter(call=>call.path.endsWith("/open_related")).length,before+1);
    const rows=()=>calls.filter(call=>call.path.includes("fixture.line/web_search_read"));
    assert.deepEqual(rows().at(-1).body.domain,baseDomain);
    assert.deepEqual(rows().at(-1).body.context,{lang:"en_US",tz:"UTC",allowed_company_ids:[1],active_model:"fixture.parent",active_id:5,active_ids:[5],default_name:"Default fixture line"});
    await page.getByRole("button",{name:"Next page",exact:true}).click();
    await page.waitForFunction(()=>document.querySelector('.list-table tbody tr')?.getAttribute('data-id')==='71');
    await rendered(page.locator(".list-table"),"related-next-page");
    assert.deepEqual(rows().at(-1).body.domain,baseDomain);
    await page.getByRole("button",{name:"Visible fixture line",exact:true}).click();
    await rendered(page.locator('input[name="name"]'),"related-form");
    assert.equal(new URL(page.url()).searchParams.get("method_action"),identity);
    if(officeMenuProbe){
      const trail=page.locator('.form-breadcrumb-path');
      assert.equal(await trail.locator('.breadcrumb-back').textContent(),current.name);
      assert.ok((await trail.innerText()).includes("Sequences and Identifiers"));
      if(width===1440){
        const metrics=await page.locator('.office-navigation').evaluate(nav=>({client:nav.clientWidth,scroll:nav.scrollWidth,labels:[...nav.querySelectorAll('summary > span')].map(n=>({text:n.textContent,width:n.getBoundingClientRect().width,height:n.getBoundingClientRect().height,line:parseFloat(getComputedStyle(n).lineHeight)}))}));
        assert.ok(metrics.scroll<=metrics.client+1,JSON.stringify(metrics));
        assert.ok(metrics.labels.every(x=>x.width>=80&&x.height<=48),JSON.stringify(metrics));
        checks.push({scenario:"bounded nested sidebar labels",metrics,passed:true});
      }
      await trail.locator('.breadcrumb-back').click();await rendered(page.locator('.list-table'),"breadcrumb-related-list");assert.deepEqual(rows().at(-1).body.domain,baseDomain);
      checks.push({scenario:"source menu and actual related action breadcrumb retain filtered back route",width,passed:true});continue;
    }
    await page.goBack();await rendered(page.locator(".list-table"),"related-back");assert.deepEqual(rows().at(-1).body.domain,baseDomain);
    await page.goForward();await rendered(page.locator('input[name="name"]'),"related-forward");
    assert.equal(calls.filter(call=>call.path.endsWith("/open_related")).length,before+1);
    await page.goBack();await rendered(page.locator(".list-table"),"related-search");
    await page.getByRole("searchbox",{name:"Search records",exact:true}).fill("Additional query");
    await page.getByRole("searchbox",{name:"Search records",exact:true}).press("Enter");
    await rendered(page.getByRole("button",{name:"Clear search filters",exact:true}),"related-filtered");
    await Promise.all([page.waitForResponse(response=>response.url().includes("fixture.line/web_search_read")),page.getByRole("button",{name:"Clear search filters",exact:true}).click()]);
    await page.waitForFunction(()=>document.querySelector('.search-clear-btn')?.style.display==='none');
    assert.deepEqual(rows().at(-1).body.domain,baseDomain);
    const beforeReload=rows().length;
    await page.reload();await rendered(page.locator(".client-error-dialog"),"expired-action");
    assert.equal(await page.locator("#client-error-message").textContent(),"This related action has expired. Open its originating menu again.");
    assert.equal(rows().length,beforeReload);
    assert.equal(await page.locator(".list-table").count(),0);
    assert.equal(await page.locator(".method-action-recovery").getAttribute("href"),"/erp/action-20");
    checks.push({scenario:"scoped list/pages/form/history/search-clear and explicit reload expiry",width,passed:true});
  }
  if(!officeMenuProbe){
  denyTarget=true;
  await page.goto(origin+"/erp/action-20?id=5&view_type=form");
  await rendered(page.getByRole("button",{name:"Related records",exact:true}),"denial-source");
  const before=calls.filter(call=>call.path.endsWith("/open_related")).length;
  await page.getByRole("button",{name:"Related records",exact:true}).click();
  await rendered(page.locator(".client-error-dialog"),"destination-denial");
  assert.equal(await page.locator("#client-error-message").textContent(),"Related destination denied by fixture.");
  const close=page.getByRole("button",{name:"Close",exact:true});
  assert.equal(await close.evaluate(node=>node===document.activeElement),true,"error close receives native modal focus");
  await close.press("Enter");
  await page.locator(".client-error-dialog").waitFor({state:"detached"});
  assert.equal(await page.getByRole("button",{name:"Related records",exact:true}).isDisabled(),false);
  assert.equal(calls.filter(call=>call.path.endsWith("/open_related")).length,before+1);
  checks.push({scenario:"destination denial and focused native Enter dismissal leave source interactive without method replay",passed:true});
  }
  if(serverError)throw serverError;assert.deepEqual(errors,[]);result.status="PASS";
}catch(error){result.failure=String(error.stack??error);result.page_text=await page.locator("body").innerText();await page.screenshot({path:path.join(output,"failure.png")});if(process.env.MOLI_DIAGNOSTIC)await diagnose("after failure screenshot");throw error;}
finally{result.calls=calls;result.page_errors=errors;await writeFile(path.join(output,"result.json"),JSON.stringify(result,null,2)+"\n");await browser.close();await new Promise(resolve=>server.close(resolve));}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// JSON2, authentication and assets are served by the actual task native server.
// Only the first JS asset body is replaced with the actual baseline bundle to simulate a cached tab.
import assert from "node:assert/strict";
import {readFile,mkdir,writeFile} from "node:fs/promises";
import {hostname} from "node:os";
import path from "node:path";
import {createHash} from "node:crypto";
import {chromium} from "playwright";
let input="";for await(const chunk of process.stdin)input+=chunk;
const {origin,cookies,action,oldAssets}=JSON.parse(input);
const output=process.env.PANEL_BROWSER_OUTPUT;
if(!output)throw new Error("PANEL_BROWSER_OUTPUT is required");
await mkdir(output); // Refuse pre-existing evidence, including a stale successful receipt.
const manifest=JSON.parse(await readFile(path.join(oldAssets,"manifest.json"),"utf8"));
const old=manifest.assets.find(asset=>asset.name==="client.js");assert.ok(old);
const body=await readFile(path.join(oldAssets,old.path));
assert.equal(createHash("sha256").update(body).digest("hex"),old.sha256);
const executablePath=process.env.CHROME_BIN??(process.platform==="darwin"?"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome":"/usr/bin/google-chrome");
const browser=await chromium.launch({executablePath,headless:true,args:["--no-sandbox","--disable-dev-shm-usage"]});
let page;
const stamp={host:hostname(),utc:new Date().toISOString(),node:process.version,browser:await browser.version()};
try {
 const context=await browser.newContext();
 await context.addCookies(cookies.split("; ").map(cookie=>{const i=cookie.indexOf("=");return {name:cookie.slice(0,i),value:cookie.slice(i+1),url:origin};}));
 page=await context.newPage();let navigations=0,panelCalls=0;const responses=[],routeEvents=[];
 page.on("framenavigated",frame=>{if(frame===page.mainFrame())routeEvents.push({url:frame.url(),utc:new Date().toISOString()});});
 page.on("request",request=>{
   // Same-document history replacement also emits framenavigated. Count actual document
   // navigation chains instead, so successful SPA mounting is not mistaken for a reload.
   if(request.isNavigationRequest()&&request.frame()===page.mainFrame()&&request.redirectedFrom()===null)navigations++;
   if(/search_panel_select_(?:multi_)?range/.test(request.url()))panelCalls++;
 });
 page.on("response",response=>{if(/\/(?:get_views|erp_load_web_action_views)$/.test(response.url()))responses.push(response);});
 const assetPattern=/\/erp\/assets\/.*\.js(?:\?.*)?$/;
 const stale=async route=>route.fulfill({status:200,contentType:"text/javascript",body});
 await page.route(assetPattern,stale);
 await page.goto(`${origin}/erp/action-${action}`,{waitUntil:"networkidle"});
 await page.getByText("Unsupported view renderer: web.list_sections_v1.",{exact:true}).waitFor({state:"visible"});
 assert.equal(navigations,1,"old client must not reload automatically");assert.equal(panelCalls,0);
 await page.screenshot({path:path.join(output,"old-client-visible-refusal.png")});
 assert.ok(responses.length>0);const answer=await responses[0].json();const wire=answer.views?.views?answer.views:answer;
 assert.ok(wire.views.some(view=>view.renderer==="web.list_sections_v1"));
 await page.unroute(assetPattern,stale);
 await page.reload({waitUntil:"networkidle"});
 await page.getByText("Search panel categories and filters require a later release.",{exact:true}).waitFor({state:"visible"});
 assert.equal(navigations,2,"manual reload recovers without another automatic navigation");assert.equal(panelCalls,0);
 await page.screenshot({path:path.join(output,"manual-reload-recovered.png")});
 // Independent new-client fault injection exercises the mounted app's bounded recovery.
 // The underlying metadata is still fetched from the native server; only its panel version is corrupted.
 let corrupted=0;
 await page.route(/\/(?:get_views|erp_load_web_action_views)$/,async route=>{
   // APIRequestContext does not synthesize browser Sec-Fetch headers. Preserve the
   // intercepted same-origin browser request when forwarding to the real native boundary.
   const headers=await route.request().allHeaders();
   const response=await route.fetch({headers:{...headers,"sec-fetch-site":"same-origin"}});
   assert.equal(response.status(),200,"native forwarded metadata must succeed");
   const answer=await response.json();const metadata=answer.views?.views?answer.views:answer;
   for(const view of metadata.views)for(const node of view.nodes)if(node.search_panel?.kind==="sections"){node.search_panel.version=2;corrupted++;}
   await route.fulfill({response,json:answer});
 });
 await page.reload({waitUntil:"domcontentloaded"});
 await page.getByText("The application changed. Reload the page to continue.",{exact:true}).waitFor({state:"visible"});
 await page.waitForLoadState("networkidle");
 assert.equal(navigations,4,"one manual fault injection reload and exactly one automatic recovery");
 assert.ok(corrupted>=2);assert.equal(panelCalls,0);
 assert.equal(await page.evaluate(()=>sessionStorage.getItem("erp.panel-sections-v1.reload-consumed")),"1");
 const result={...stamp,case:"old-bundle-new-native-panel",old_bundle_sha256:old.sha256,old_client_visible_refusal:true,manual_reload_recovered:true,new_client_once_only_recovery:true,persistent_mismatch_visible:true,navigations,route_events:routeEvents,panel_calls:panelCalls};
 await writeFile(path.join(output,"result.json"),JSON.stringify(result,null,2)+"\n");
 console.log(JSON.stringify(result));
 await context.close();
} catch(error) {
 const failure={...stamp,status:"FAIL",message:String(error)};
 await writeFile(path.join(output,"failure.json"),JSON.stringify(failure,null,2)+"\n");
 if(page)try {await page.screenshot({path:path.join(output,"failure.png")});}catch(capture){console.error("Failure screenshot unavailable:",capture);}
 throw error;
} finally {await browser.close();}

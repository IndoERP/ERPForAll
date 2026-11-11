/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Native metadata and rows; no intercepted response or mocked backend.
import assert from "node:assert/strict";
import {mkdir,writeFile} from "node:fs/promises";
import {hostname} from "node:os";
import path from "node:path";
import {chromium} from "playwright";
let input="";for await(const chunk of process.stdin)input+=chunk;
const {origin,cookies,action}=JSON.parse(input);
const output=process.env.PANEL_BROWSER_OUTPUT ? `${process.env.PANEL_BROWSER_OUTPUT}-list` : undefined;
if(!output)throw new Error("PANEL_BROWSER_OUTPUT is required");
await mkdir(output);
const executablePath=process.env.CHROME_BIN??(process.platform==="darwin"?"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome":"/usr/bin/google-chrome");
const browser=await chromium.launch({executablePath,headless:true,args:["--no-sandbox","--disable-dev-shm-usage"]});
try {
 const context=await browser.newContext();
 await context.addCookies(cookies.split("; ").map(cookie=>{const i=cookie.indexOf("=");return {name:cookie.slice(0,i),value:cookie.slice(i+1),url:origin};}));
 const page=await context.newPage(); let panelCalls=0; const domains=[];
 page.on("request",request=>{
  if(/search_panel_select_(?:multi_)?range/.test(request.url()))panelCalls++;
  if(request.url().endsWith("web_search_read"))domains.push(request.postDataJSON().domain);
 });
 await page.goto(`${origin}/erp/action-${action}`,{waitUntil:"networkidle"});
 await page.getByRole("status").filter({hasText:"Search panel section 0 has unsupported panel syntax; additional syntax requires a later release."}).waitFor();
 await page.getByText("Panel list remains usable",{exact:true}).waitFor();
 assert.ok(await page.getByRole("button",{name:"New",exact:true}).isEnabled());
 assert.equal(panelCalls,0); assert.ok(domains.length>0);
 assert.ok(domains.every(domain=>!JSON.stringify(domain).includes("priority")),"unsupported panel must not add a category predicate");
 await page.screenshot({path:path.join(output,"unsupported-panel-list.png")});
 const result={host:hostname(),utc:new Date().toISOString(),node:process.version,browser:await browser.version(),list_row_visible:true,new_control_enabled:true,panel_calls:panelCalls,panel_predicate_absent:true};
 await writeFile(path.join(output,"result.json"),JSON.stringify(result,null,2)+"\n");
 console.log(JSON.stringify(result)); await context.close();
} finally {await browser.close();}

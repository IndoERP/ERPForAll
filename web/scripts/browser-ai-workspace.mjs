/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Exercise the actual addon in Chrome with deterministic HTTP/RPC replies, without PostgreSQL.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { build } from "esbuild";
import { chromium } from "playwright";

const root = fileURLToPath(new URL("../../", import.meta.url));
const addon = path.join(root, "addons/ai_base/static/src");
const compiled = await build({
  stdin: { contents: `
    import {workspace} from "${addon}/workspace.ts";
    import {assistantChat} from "${addon}/assistant-chat.ts";
    const rpc = {call:async (model, method, args) => {
      const r = await fetch("/rpc", {method:"POST", body:JSON.stringify({model,method,args})});
      if (!r.ok) throw new Error("RPC failed");
      return r.json();
    }};
    const limits = {turns:8,bytes:16384,message_bytes:4096,saved_sessions:true,workspace_action_id:23};
    let active;
    const run = job => job().catch(error => {throw error});
    async function openStudio() {
      if (active) return;
      sessionStorage.setItem("fixture.page","studio");
      active = await workspace({currentCompanyId:7,userId:19,csrfToken:"fixture",companies:[],rpc,host:document.querySelector("main"),
        run,open: async action => {window.opened=action},action:{params:{company_id:7,sources:[],limit:10,history:limits}}});
    }
    const dock = assistantChat({rpc,userId:19,csrfToken:"fixture",context:{allowed_company_ids:[7]},run,
      navigate:async route=>{ if(route.action === "23") await openStudio(); }
    },async (url,init)=> url === "/erp/ai/assistant/status" ? Response.json({available:true,history:limits}) : fetch(url,init));
    document.body.prepend(dock.node);
    if (sessionStorage.getItem("fixture.page") === "studio") await openStudio();
    window.leaveStudio = () => {active.dispose(); active=undefined;sessionStorage.removeItem("fixture.page");};

  `, resolveDir: root, loader: "ts" },
  bundle: true, write: false, format: "esm", target: "es2023",
  alias: {
    "@erpforall/client": path.join(root, "addons/web/static/src/client/public.ts"),
    "@erpforall/elang/abi": path.join(root, "web/src/runtime/abi-impl.ts"),
    "@erpforall/elang": path.join(root, "web/src/index.ts"),
    "@erpforall/expr": path.join(root, "web/packages/expr/src/index.ts"),
    "@erpforall/human-templates": path.join(root, "web/tests/fixtures/client-landmark-templates.ts"),
  },
});
const css = (await Promise.all([
  "addons/web/static/src/client/client.css", "addons/theme_office_blue/static/src/theme.css",
  "addons/ai_base/static/src/assistant-chat.css", "addons/ai_base/static/src/workspace.css", "addons/ai_base/static/src/presentation.css", "addons/ai_base/static/src/canvas/canvas-studio.css",
].map(file => readFile(path.join(root,file),"utf8")))).join("\n");
const pending = {name:"crm.lead.action_create_quotation",model:"crm.lead",ids:[6],status:"awaiting_approval",approval_request_id:24};
const done = {...pending,status:"completed",result:{res_model:"sale.order",res_id:32}};
const brief = {canvas_type:"brief",title:"Purchasing rollout",description:"Pilot purchasing with the Jakarta team before opening the workflow to every branch.",data:{sections:[
  {kind:"heading",text:"Pilot scope"},{kind:"paragraph",text:"Start with one purchasing team. Verify supplier records, approval limits and the opening order before expanding to other branches."},
  {kind:"checklist",items:[{label:"Review supplier records",state:"completed"},{label:"Confirm purchase approval limits",state:"active"},{label:"Run the first order",state:"pending"}]},
]}};
const timeline = {canvas_type:"timeline",title:"Pilot milestones",description:"Release in three stages.",data:{items:[
  {label:"Validate master data",when:"Week 1",detail:"Purchasing reviews the supplier list."},{label:"Process pilot orders",when:"Week 2"},{label:"Review and expand",when:"Week 3"},
]}};
const chart = {canvas_type:"chart",title:"Purchase orders by stage",description:"ERP · purchase.order",
  data:{source_model:"purchase.order",source_domain:[],series:[{label:"Request",value:16},{label:"Approved",value:12},{label:"Ordered",value:22}],
    columns:["state","id:count"],records:[{state:"Request","id:count":16},{state:"Approved","id:count":12},{state:"Ordered","id:count":22}]}};
const salesQuestion = "Show monthly sales by currency";
const sourceTable = "<table><tr><th>Reference</th></tr><tr><td>" + "2026110123456789".repeat(6) + "</td></tr></table>";
const salesReply = [
  "**Monthly Sales by Currency**", "",
  "| Month | Currency | Confirmed Sales | Orders |", "| --- | --- | ---: | ---: |",
  "| November 2026 | USD | 53,047.92 | 17 |", "| **Total** | **USD** | **53,047.92** | **17** |", "",
  "1. Review sales", "   - Confirm the period", "",
  "```html", sourceTable, "```", "",
  '<p><em>End of report.</em><img src="/external-resource"><iframe src="/external-resource"></iframe></p>',
].join("\n");
let received = [], approvals = 0;
const server = createServer(async (req,res) => {
  if (req.url === "/fixture.js") {
    res.writeHead(200,{"Content-Type":"text/javascript"}).end(compiled.outputFiles[0].contents); return;
  }
  if (req.url === "/fixture.css") { res.writeHead(200,{"Content-Type":"text/css"}).end(css); return; }
  if (req.method === "POST") {
    const chunks = [];
    for await (const part of req) chunks.push(part);
    const value = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    let reply;
    if (req.url === "/erp/ai/assistant/turn") {
      received.push(value);
      if (value.message === salesQuestion) reply = {session_id:18,text:salesReply};
      else reply = value.message === "Prepare a rollout brief and timeline"
        ? {session_id:18,text:"The purchasing pilot has three milestones. Start with the supplier review.",canvases:[brief,chart,timeline],canvas:timeline}
        : {session_id:18,text:"Quotation approval requested.",operations:[pending]};
    } else if (req.url === "/erp/ai/assistant/resume" && value.approval_request_id === 24 && value.company_id === 7) {
      approvals++; reply = done;
    } else if (req.url === "/rpc" && value.method === "search_sessions") {
      reply = [{id:18, title:salesQuestion, state:"open", last_event_at:"2026-11-28T12:00:00Z", anchor:null}];
    } else if (req.url === "/rpc" && value.method === "resume_session" && value.args.id === 18) {
      reply = {id:18, last_seq:2, anchor:null};
    } else if (req.url === "/rpc" && value.method === "conversation_history" && value.args.id === 18) {
      if (received.at(-1)?.message === salesQuestion) reply = {id:18,messages:[
        {role:"user",text:salesQuestion},{role:"assistant",text:salesReply}
      ]};
      else reply = {id:18,messages:[{role:"user",text:"Create quotation"},{role:"assistant",text:"Quotation approval requested.",operations:[approvals ? done : pending]}]};
    } else { res.writeHead(400).end(); return; }
    res.writeHead(200,{"Content-Type":"application/json"}).end(JSON.stringify(reply)); return;
  }
  const theme = req.url === "/blue" ? ' data-office-theme="office-blue"' : "";
  res.writeHead(200,{"Content-Type":"text/html"}).end(`<!doctype html><html lang="en"${theme}><head><title>AI workspace</title><style>main{height:100dvh;min-width:0}</style><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/fixture.css"></head><body><main></main><script src="/fixture.js" type="module"></script></body></html>`);
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
let browser;
try {
  browser = await chromium.launch({headless:true,...(process.env.CHROME_BIN && {executablePath:process.env.CHROME_BIN})});
  for (const theme of ["default","blue"]) for (const width of [390,1280]) {
    received = []; approvals = 0;
    const page = await browser.newPage({viewport:{width,height:850}});
    const errors = [], embeddedRequests = [];
    page.on("request", request => {
      if (new URL(request.url()).pathname === "/external-resource") embeddedRequests.push(request.url());
    });
    page.on("pageerror", e => errors.push(e.message));
    await page.goto(`http://127.0.0.1:${server.address().port}/${theme}`);
    const input = page.getByRole("textbox", {name:"Message to AI Assistant"});
    await page.getByRole("button",{name:"AI Assistant",exact:true}).click();
    await input.fill("Create quotation");
    await page.getByRole("button",{name:"Open Canvas Studio"}).click();
    await page.locator("main textarea").waitFor();
    assert.equal(await input.count(),1);
    assert.equal(await input.inputValue(),"Create quotation");
    assert.equal(await page.locator("#ai-assistant-dock").count(),0);
    await input.press("Enter");
    await page.getByRole("button",{name:"View approval"}).click();
    assert.equal(await page.evaluate(() => window.opened.res_id),24);
    await page.reload();
    await page.getByRole("button",{name:"Check approval"}).click();
    await page.getByRole("button",{name:"Open record"}).click();
    assert.deepEqual(await page.evaluate(() => [window.opened.res_model,window.opened.res_id]),["sale.order",32]);
    await input.fill("Show its status");
    await input.press("Enter");
    await page.waitForFunction(() => document.querySelectorAll(".ai-operation").length === 2);
    assert.equal(received.length,2);
    assert.equal(received[1].session_id,18);
    assert.equal(received[1].history,undefined);
    assert.equal(approvals,1);
    await page.getByRole("button",{name:"New chat"}).click();
    assert.equal(await page.getByRole("log").innerText(),"");
    assert.equal(await input.evaluate(el => document.activeElement === el),true);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),true);
    await input.fill("Prepare a rollout brief and timeline");
    await input.press("Enter");
    await page.getByRole("button",{name:"Open Purchasing rollout",exact:true}).click();
    await page.getByRole("heading",{name:"Pilot scope"}).waitFor();
    const focus = page.getByRole("button",{name:"Enter focus mode"});
    await focus.click();
    await page.getByRole("dialog",{name:"Canvas Studio"}).waitFor();
    await page.keyboard.press("Escape");
    await focus.waitFor();
    if (process.env.ERP_CANVAS_SCREENSHOTS) {
      await mkdir(process.env.ERP_CANVAS_SCREENSHOTS,{recursive:true});
      await page.screenshot({path:path.join(process.env.ERP_CANVAS_SCREENSHOTS,theme+"-"+width+".png")});
    }
    if (width < 800) await page.getByRole("button",{name:"Chat",exact:true}).click();
    await page.getByRole("button",{name:"Open Purchase orders by stage",exact:true}).click();
    await page.getByRole("button",{name:"Open source records"}).click();
    assert.equal(await page.evaluate(()=>window.opened.res_model),"purchase.order");
    if (width < 800) await page.getByRole("button",{name:"Chat",exact:true}).click();
    await page.getByRole("button",{name:"Open Pilot milestones",exact:true}).click();
    await page.getByText("Validate master data",{exact:true}).waitFor();
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),true);
    await page.evaluate(()=>window.leaveStudio());
    assert.equal(await page.locator("#ai-assistant-dock").count(),0);
    await page.getByRole("button",{name:"AI Assistant",exact:true}).click();
    assert.equal(await input.count(),1);
    assert.equal(await page.locator("#ai-assistant-dock").count(),1);
    await input.fill(salesQuestion);
    await input.press("Enter");
    async function checkSalesReply() {
      const reply = page.getByRole("log").locator(".ai-reply-prose").filter({hasText:"Monthly Sales by Currency"});
      const table = reply.getByRole("table").first();
      await table.waitFor();
      assert.deepEqual(await table.getByRole("columnheader").allTextContents(),["Month","Currency","Confirmed Sales","Orders"]);
      assert.deepEqual(await table.getByRole("row").last().getByRole("cell").allTextContents(),["Total","USD","53,047.92","17"]);
      assert.equal(await table.locator("strong").first().evaluate(el => getComputedStyle(el).display),"inline");
      assert.equal(await reply.locator("ol ul li").innerText(),"Confirm the period");
      assert.equal(await reply.getByText("Lihat sumber",{exact:true}).count(),1);
      assert.equal(await reply.locator("img,iframe").count(),0);
      if (width === 390) {
        const wide = reply.locator(".ai-reply-table").last();
        assert.ok(await wide.evaluate(el => el.scrollWidth > el.clientWidth));
        assert.ok(await reply.evaluate(el => el.closest('[role="log"]').scrollWidth <= el.closest('[role="log"]').clientWidth + 1));
      }
    }
    await checkSalesReply();
    const summary = page.getByText("Lihat sumber",{exact:true});
    await summary.focus();
    await page.keyboard.press("Enter");
    assert.equal(await page.locator("details[open] code").innerText(),sourceTable);
    await page.keyboard.press("Enter");
    await page.getByRole("button",{name:"Open Canvas Studio"}).click();
    await checkSalesReply();
    await page.reload();
    await checkSalesReply();
    assert.equal(await input.count(),1);
    await page.getByRole("button",{name:"New chat",exact:true}).click();
    await page.getByRole("button",{name:"History",exact:true}).click();
    await page.getByRole("button",{name:"Resume " + salesQuestion,exact:true}).click();
    await checkSalesReply();
    assert.deepEqual(embeddedRequests,[]);
    assert.deepEqual(errors,[]);
    console.log(`${theme} ${width}px: submit, reload, approval, open record, follow-up new chat, multi-output Canvas, focus mode, formatted replies in dock/workspace/history and single-composer handoff passed`);
    await page.close();
  }
} finally {
  await browser?.close();
  await new Promise(resolve => server.close(resolve));
}

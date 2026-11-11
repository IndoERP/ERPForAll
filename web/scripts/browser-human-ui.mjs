/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Isolated compiled-asset browser rendering test of compiled assets against explicit HTTP protocol fixtures.
// This is NOT backend, authentication, persistence, deployment or whole-case HUMAN acceptance evidence.
import assert from "node:assert/strict";
import { waitForReadiness } from "./browser-readiness.mjs";
import { access, readFile, writeFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { constants } from "node:fs";
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { tmpdir, hostname } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.CLIENT_BROWSER_OUTPUT;
/// (partial: compiled fixture palettes; live served first-paint path is separate)
const theme = process.env.CLIENT_BROWSER_THEME ?? "light";
const officeBlue = process.env.CLIENT_BROWSER_OFFICE_BLUE === "1";
// Moli supports real narrow viewport layout, but explicitly rejects mobile device emulation.
const viewportOnly = process.env.CLIENT_BROWSER_VIEWPORT_ONLY === "1";
const listOnly = process.env.CLIENT_BROWSER_LIST_ONLY === "1";
const dialogProbe = process.env.CLIENT_BROWSER_DIALOG_PROBE === "1";
if (listOnly && !officeBlue) throw new Error("CLIENT_BROWSER_LIST_ONLY requires Office Blue.");
let officeChoice = "office-blue", officeInstalled = true, missingOfficeCss = false;
assert.ok(["light", "dark", "auto-light", "auto-dark"].includes(theme), "CLIENT_BROWSER_THEME must be light, dark, auto-light, or auto-dark");
if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to the isolated evidence directory.");
await mkdir(path.dirname(path.resolve(output)), { recursive: true });
try { await mkdir(output); }
catch (error) {
  if (error.code === "EEXIST") throw new Error(`Evidence directory already exists: ${output}`, { cause: error });
  throw error;
}
const chromeBin = process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const remoteCdp = process.env.CLIENT_BROWSER_CDP;
if (!remoteCdp) {
  try { await access(chromeBin, constants.X_OK); }
  catch (error) { throw new Error(`Chrome executable is unavailable: ${chromeBin}`, { cause: error }); }
}
const manifest = JSON.parse(await readFile(path.join(root, "web/dist/human-ui/manifest.json"), "utf8"));
const assets = new Map();
for (const asset of manifest.assets) {
  assert.match(asset.path, /^client\.[0-9a-f]{64}\.(js|css)$/);
  const data = await readFile(path.join(root, "web/dist/human-ui", asset.path));
  assert.equal(createHash("sha256").update(data).digest("hex"), asset.sha256);
  assets.set("/erp/assets/" + asset.path, { data, type: asset.content_type });
}
const js = manifest.assets.find(asset => asset.name === "client.js"), css = manifest.assets.find(asset => asset.name === "client.css");
for (const name of ["logo.svg", "logo-dark.svg"]) {
  assets.set("/erp/brand/" + name, { data: await readFile(path.join(root, "crates/erp-server/brand/erpforall-" + name)), type: "image/svg+xml" });
}
if (officeBlue) assets.set("/theme_office_blue/static/src/theme.css", { data: await readFile(path.join(root, "addons/theme_office_blue/static/src/theme.css")), type: "text/css; charset=utf-8" });
const bootstrap = { wire_major: 1, registry_version: "fixture-r1", asset_version: "fixture-a1", identity: { uid: 2, name: "Rendering fixture user", internal: true }, companies: [{ id: 1, name: "Rendering fixture company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { lang: "en_US", tz: "UTC", decimal_point: ".", thousands_sep: ",", grouping: [3, 0], date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "fixture-not-a-real-token" }, renderers: [{ key: "web.list", schema_major: 1 }, { key: "web.form", schema_major: 1 }] };
const scalar = (type, label, required = false, extra = {}) => ({type, label, required, readonly:false, selection:null, relation:null, relation_field:null, ...extra});
const fields = {
 name:scalar("char","Title",true), body:scalar("text","Description"),
 line_count:scalar("integer","Lines",false,{readonly:true}),
 priority:scalar("selection","Priority",false,{selection:[["normal","Normal"],["high","High"]]}),
 owner_id:scalar("many2one","Owner",true,{relation:"res.users"}),
 company_id:scalar("many2one","Company",true,{relation:"res.company"}),
 active:scalar("boolean","Active"), internal_memo:scalar("text","Internal Memo"),
 scheduled_date:scalar("date","Scheduled date",true), reference:scalar("char","Reference",false,{readonly:true}),
 create_uid:scalar("many2one","Created by",false,{readonly:true,relation:"res.users"}),
 create_date:scalar("datetime","Created on",false,{readonly:true}),
 write_uid:scalar("many2one","Last edited by",false,{readonly:true,relation:"res.users"}),
 write_date:scalar("datetime","Last edited on",false,{readonly:true})
};
const formField = name => ({kind:"field", force_save: false,field:{model:"acceptance_notes.note",name},widget:name==="name"?"acceptance_notes.note_title":fields[name].type,options:{},modifiers:{readonly:null,required:null,invisible:null},label:fields[name].label});
const formNodes=[];
function arena(node) {
 const index=formNodes.length;formNodes.push({});
 const {children,...props}=node;
 formNodes[index]={...props,...(children?{children:children.map(arena)}:{})};return index;
}
const lendButton = {kind:"button",name:"lend",button_type:"object",label:"Lend",variant:"primary",confirm:"Lend this book?",modifiers:{readonly:null,required:null,invisible:{schema:1,nodes:[
 {kind:"name",name:"priority"},{kind:"literal",value:{kind:"str",value:"normal"}},{kind:"compare",left:0,ops:["not_eq"],comparators:[1]}]}}};
arena({kind:"form",record_mode:"record",children:[{kind:"header",children:[lendButton,{...formField("priority"),widget:"statusbar",options:{visible:null}}]},{kind:"sheet",children:[
 {kind:"button_box",children:[{kind:"stat_button",name:"inspect_lines",button_type:"object",label:"Lines",variant:"secondary",confirm:null,modifiers:{readonly:null,required:null,invisible:null},children:[{...formField("line_count"),widget:"statinfo"}]}]},
 {kind:"title",children:[{kind:"heading",level:1,children:[formField("name")]}]}, {kind:"group",label:"",columns:4,colspan:2,children:[
  {kind:"group",label:"Details",columns:2,colspan:2,children:[formField("priority"),formField("owner_id"),formField("scheduled_date"),formField("reference")]},
  {kind:"group",label:"Organization",columns:2,colspan:2,children:[formField("company_id"),formField("active")]}
 ]}, {kind:"notebook",children:[
  {kind:"page",name:"description",label:"Description",children:[formField("body")]},
  {kind:"page",name:"internal",label:"Internal Memo",children:[formField("internal_memo")]}
 ]}
]},{kind:"aside",children:[{kind:"group",label:"Record information",columns:2,colspan:2,children:[
 formField("create_uid"),formField("create_date"),formField("write_uid"),formField("write_date")
]}]},{kind:"chatter",reload_on_post:true,reload_on_attachment:false,reload_on_activity:false,reload_on_follower:false,open_attachments:false,has_activities:true}]});
const description = {wire_major:1,registry_version:"fixture-r1",description_version:"fixture-d1",model:"acceptance_notes.note",fields:{"acceptance_notes.note":fields},notices:[],views:[
 {identity:{kind:"stored",id:1},type:"list",schema_major:1,renderer:"web.list",root:0,nodes:[{kind:"list",children:[1,2],page_limit:80,...(officeBlue ? {search_panel:{kind:"selection",field:{model:"acceptance_notes.note",name:"priority"},label:"Priority",enable_counters:true,expand:true,default_value:null,notice:null}} : {})},{...formField("name"),widget:"char"},formField("priority")]},
 {identity:{kind:"stored",id:2},type:"form",schema_major:1,renderer:"web.form",root:0,nodes:formNodes}
]};
const recordValues=()=>({id:5,line_count:lineCount,name:note,body:"Draft description",priority:priorityValue,owner_id:{id:2,display_name:"Rendering fixture user"},company_id:{id:1,display_name:"Rendering fixture company"},active:true,internal_memo:"",scheduled_date:"2026-11-01",reference:"Readonly fixture reference",create_uid:{id:2,display_name:"Rendering fixture user"},create_date:"2026-11-01 09:00:00",write_uid:{id:2,display_name:"Rendering fixture user"},write_date:"2026-11-01 09:30:00"});
const settingsDescription = { wire_major: 1, registry_version: "fixture-r1", description_version: "settings-d1", model: "ir.config.setting", fields: { "ir.config.setting": {} }, notices: [], views: [{ identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "web.settings", options: { scope_kinds: ["global", "company"], app: null }, modifiers: { readonly: null, required: null, invisible: null } }] }] };
const addonsDescription = { ...settingsDescription, model: "ir.module.module", fields: { "ir.module.module": {} }, views: [{ ...settingsDescription.views[0], identity: { kind: "stored", id: 4 }, nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "web.addons", options: {}, modifiers: { readonly: null, required: null, invisible: null } }] }] };
let rejectNextAction = false;
const permissionMessage = "You do not have access to this view. Return to an available menu or ask your administrator.";
const deniedActions = [];
let delayNextRead = false;
let selectionFixture = false;
let note = "Browser fixture note";
let priorityValue = "normal";
let lineCount = 17;
const inspectCalls = [];
const lendCalls = [];
const writes = [], calls = [], searchDomains = [], pageReads = [], groupDomains = [];
let rejectNextCount = false;
function html(login) {
  const data = login ? { csrf: "fixture-not-a-real-token", database: "fixture", redirect: "/erp", error: "<img src=x onerror=alert(1)>" } : bootstrap;
  return `<!doctype html><html lang="en"${theme.startsWith("auto-") ? "" : ` data-theme="${theme}"`}><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Compiled client rendering fixture</title></head><body><div id="erp-root"></div><script type="application/json" id="${login ? "erp-login" : "erp-bootstrap"}">${JSON.stringify(data).replaceAll("<", "\\u003c")}</script><script type="module" src="/erp/assets/${js.path}"></script></body></html>`;
}
const server = createServer(async (request, response) => {
  try {
    const pathname = new URL(request.url, "http://fixture").pathname;
    response.setHeader("Cache-Control", "no-store");
    response.setHeader("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'");
    if (officeBlue && pathname === "/theme_office_blue/static/src/theme.css" && (missingOfficeCss || !officeInstalled)) { response.statusCode = 404; response.end(); return; }
    const asset = assets.get(pathname);
    if (asset) { response.setHeader("Content-Type", asset.type); response.end(asset.data); return; }
    if (request.method === "GET" && ["/erp/login", "/erp", "/erp/action-20", "/erp/action-21", "/erp/action-22"].includes(pathname)) {
      selectionFixture = pathname === "/erp/action-20" && new URL(request.url, "http://127.0.0.1").searchParams.get("view_type") === "list";
      response.setHeader("Content-Type", "text/html; charset=utf-8"); response.end(html(pathname === "/erp/login")); return;
    }
    if (request.method !== "POST" || !pathname.startsWith("/erp/json/2/")) { response.statusCode = 404; response.end(); return; }
    let input = ""; for await (const chunk of request) { input += chunk; if (input.length > 8192) throw new Error("Fixture request exceeds bound"); }
    const body = JSON.parse(input); calls.push(pathname);
    let value;
    if (pathname.endsWith("/erp_load_web_menus")) { value = { wire_major: 1, registry_version: "fixture-r1", roots: [10, 11, 12, 13], nodes: [{ id: 10, parent_id: null, children: [], label: "Notes", icon: "lucide:notebook-pen", action_id: 20 }, { id: 11, parent_id: null, children: [], label: "Settings", icon: "lucide:settings", action_id: 21 }, { id: 12, parent_id: null, children: [], label: "Addons", icon: "lucide:layers", action_id: 22 }, { id: 13, parent_id: null, children: [], label: "Restricted", icon: "lucide:shield", action_id: 23 }] };
      if (officeBlue) {
        value.roots = value.roots.map(id => id === 10 ? 14 : id);
        value.nodes[0].parent_id = 14;
        value.nodes[0].label = "All Notes";
        value.nodes.push({ id: 14, parent_id: null, children: [10], label: "Notes", icon: "lucide:notebook-pen", action_id: null });
      }
    }
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && rejectNextAction && body.ids[0] === 23) { deniedActions.push({ pathname, ids: body.ids }); response.statusCode = 403; value = { name: "erp.exceptions.AccessError", message: permissionMessage }; }
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && body.ids[0] === 22) value = { wire_major: 1, id: 22, name: "Addons", model: "ir.module.module", target: "current", views: [{ id: 4, type: "form" }], domain: null, context: null };
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && body.ids[0] === 21) value = { wire_major: 1, id: 21, name: "Settings", model: "ir.config.setting", target: "current", views: [{ id: 3, type: "form" }], domain: null, context: null };
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views"))) value = { wire_major: 1, id: 20, name: "Notes", model: "acceptance_notes.note", target: "current", views: [{ id: 1, type: "list" }, { id: 2, type: "form" }], domain: null, context: null };
    else if (pathname === "/erp/json/2/ir.module.module/get_views") value = addonsDescription;
    else if (pathname.endsWith("/erp_available_addons")) value = { wire_major: 1, addons: [{ name: "acceptance_notes", label: "Shared notes for a distributed office team", version: "1.0", state: "installed", installable: true, summary: "Long catalogue content remains readable and constrained on narrow screens.", category: "Productivity", author: "ERPForAll", application: true, website: "", license: "LGPL-3", depends: ["web"] }] };
    else if (pathname === "/erp/json/2/ir.config.setting/get_views") value = settingsDescription;
    else if (pathname === "/erp/json/2/ir.config.setting/erp_describe") value = { wire_major: 1, registry_version: "fixture-r1", scope: body.scope, declarations: [...(officeBlue && officeInstalled ? [{ key: "theme_office_blue.appearance", kind: "selection", label: "Database appearance (refresh after saving)", choices: [["default", "Default"], ["office-blue", "Office Blue"]], writable: true }] : []), { key: "acceptance_notes.enabled", kind: "boolean", label: "Enable shared notes", choices: null, writable: true }] };
    else if (pathname === "/erp/json/2/ir.config.setting/erp_read_values") value = { wire_major: 1, scope: body.scope, revision: "1", values: body.keys.map(key => key === "theme_office_blue.appearance" ? { key, value: officeChoice, source: "stored" } : { key, value: true, source: "default" }) };
    else if (officeBlue && pathname === "/erp/json/2/ir.config.setting/erp_write_values") {
      for (const change of body.changes) { assert.equal(change.key, "theme_office_blue.appearance"); officeChoice = change.value; }
      value = { wire_major: 1, scope: body.scope, revision: "2", values: [{ key: "theme_office_blue.appearance", value: officeChoice, source: "stored" }] };
    }
    else if (pathname.endsWith("/get_views")) value = description;
    else if (pathname.endsWith("/web_read")) value = [recordValues()];
    else if (pathname.endsWith("/formatted_read_group")) {
      assert.deepEqual(body.groupby, ["priority"]); assert.deepEqual(body.aggregates, ["__count"]);
      groupDomains.push(body.domain);
      if (rejectNextCount) { rejectNextCount = false; throw new Error("Explicit fixture count failure"); }
      const matches = body.domain.every(([field, op, value]) => {
        assert.equal(field, "name"); assert.equal(op, "ilike");
        return note.toLowerCase().includes(value.toLowerCase());
      });
      value = matches ? [{priority:priorityValue,__count:selectionFixture ? 2 : 1}] : [];
    }
    else if (pathname.endsWith("/web_search_read")) {
      if (delayNextRead) { delayNextRead = false; await new Promise(resolve => setTimeout(resolve, 1800)); }
      searchDomains.push(body.domain); pageReads.push({limit:body.limit,offset:body.offset,domain:body.domain});
      assert.ok(Array.isArray(body.domain));
      const matches = body.domain.every(condition => {
        assert.ok(Array.isArray(condition)); assert.equal(condition.length, 3);
        if (condition[0] === "priority") { assert.equal(condition[1], "="); return priorityValue === condition[2]; }
        assert.equal(condition[0], "name"); assert.equal(condition[1], "ilike");
        return note.toLowerCase().includes(condition[2].toLowerCase());
      });
      const records = matches ? [{ id: 5, name: note, priority: priorityValue }, ...(selectionFixture ? [{ id: 6, name: "Second fixture row", priority: priorityValue }] : [])] : [];
      value = { length: records.length, records };
    }
    else if (pathname.endsWith("/web_name_search")) {
      assert.equal(pathname,"/erp/json/2/res.users/web_name_search");
      assert.equal(body.name,"Rendering");
      assert.deepEqual(body.specification,{display_name:{}});
      value = [{id:2,display_name:"Rendering fixture user"},{id:3,display_name:"Second fixture user"}];
    }
    else if (pathname.endsWith("/erp_normalize_action_result")) value = { wire_major: 1, result: { kind: "none" } };
    else if (pathname.endsWith("/onchange")) value = { value: { name: body.values.name } };
    else if (pathname === "/erp/json/2/acceptance_notes.note/inspect_lines") { assert.deepEqual(body.ids,[5]); inspectCalls.push(body); lineCount=18; value=true; }
    else if (pathname === "/erp/json/2/acceptance_notes.note/lend") { assert.deepEqual(body.ids, [5]); lendCalls.push(body); priorityValue = "high"; value = true; }
    else if (pathname.endsWith("/web_save")) { assert.deepEqual(body.ids, [5]); note = body.vals.name; writes.push(body); value = [recordValues()]; }
    else if (pathname.endsWith("/mail.store/store_fetch")) {
      value = {
        store: JSON.stringify({
          wire: 1,
          schema: "fixture-mail-s1",
          asof: { xmin: "1", xmax: "10", xip: [], own: null },
          records: {
            "res.partner": [{ id: 1, name: "Alice Adams" }, { id: 2, name: "Bob Brown" }],
            "mail.message": [
              { id: 101, body: "<p>Kick-off notes shared with the team.</p>", message_type: "comment", date: "2026-11-09 09:15:00", author_id: 1 },
              { id: 102, body: "<p>Internal: check the loan period before approving.</p>", message_type: "comment", date: "2026-11-10 10:00:00", author_id: 2, is_internal: true },
              { id: 103, body: "", message_type: "notification", date: "2026-11-10 10:30:00", author_id: 2, tracking: JSON.stringify([{ f: 1, n: "name", t: "string", o: null, v: null, od: "Draft", vd: "Done" }]) },
            ],
          },
        }),
        topics: [
          {
            topic: "schema",
            ok: true,
            data: {
              wire: 1,
              schema: "fixture-mail-s1",
              models: {
                "res.partner": { fields: { name: { kind: "string" } } },
                // the thread's own model: the store holds the record the chatter belongs to
                "acceptance_notes.note": { fields: { name: { kind: "string" } } },
                "mail.message": {
                  fields: {
                    body: { kind: "html" },
                    subject: { kind: "string" },
                    message_type: { kind: "selection" },
                    date: { kind: "datetime" },
                    author_id: { kind: "many2one", target: "res.partner" },
                    is_internal: { kind: "bool" },
                    tracking: { kind: "string" },
                  },
                },
                "mail.message.subtype": { fields: { name: { kind: "string" }, internal: { kind: "bool" } } },
                "mail.followers": {
                  fields: {
                    res_model: { kind: "string" },
                    res_id: { kind: "int" },
                    partner_id: { kind: "many2one", target: "res.partner" },
                    subtype_ids: { kind: "x2many", target: "mail.message.subtype" },
                  },
                },
              },
            },
          },
          {
            topic: "thread",
            ok: true,
            data: {
              thread: { model: "acceptance_notes.note", id: 5, can_post: true },
              messages: {
                model: "acceptance_notes.note",
                id: 5,
                message_ids: [103, 102, 101],
                has_more_before: false,
                has_more_after: false,
              },
              followers: { count: 0, follower_ids: [], self_follower_id: null, next_after: null },
            },
          },
        ],
      };
    }
    else throw new Error("Unexpected fixture endpoint " + pathname);
    response.setHeader("Content-Type", "application/json"); response.end(JSON.stringify(pathname.endsWith("erp_load_web_action_views") && response.statusCode === 200 ? { action: value, views: value.model === "ir.module.module" ? addonsDescription : value.model === "ir.config.setting" ? settingsDescription : description } : value));
  } catch (error) { response.statusCode = 500; response.setHeader("Content-Type", "application/json"); response.end(JSON.stringify({ name: "FixtureError", message: error.message })); }
});
server.listen(0, "127.0.0.1"); await once(server, "listening");
const base = `http://127.0.0.1:${server.address().port}`, profile = await mkdtemp(path.join(tmpdir(), "erp-client-chrome-"));
const chrome = remoteCdp ? undefined : spawn(chromeBin, ["--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
let socket, sequence = 0, session;
const pending = new Map(), exceptions = [], screenshots = [], listLayouts = [], errorLayouts = [], resizeLayouts = [];
class CdpProtocolError extends Error {
  constructor(method, detail) { super(`${method}: ${detail}`); this.name = "CdpProtocolError"; this.method = method; this.detail = detail; }
}

try {
  const endpoint = remoteCdp ? (await (await fetch(new URL("/json/version", remoteCdp))).json()).webSocketDebuggerUrl : await new Promise((resolve, reject) => {
    const finish = (error, endpoint) => {
      clearTimeout(timer); chrome.off("error", failed); chrome.off("exit", exited); chrome.stderr.off("data", listening);
      if (error) reject(error); else resolve(endpoint);
    };
    const failed = error => finish(error);
    const exited = code => finish(new Error(`Chrome exited before connection: ${code}`));
    const listening = chunk => { const match = /DevTools listening on (ws:\/\/[^\s]+)/.exec(chunk.toString()); if (match) finish(undefined, match[1]); };
    const timer = setTimeout(() => finish(new Error("Chrome startup deadline exceeded")), 15000);
    chrome.once("error", failed); chrome.once("exit", exited); chrome.stderr.on("data", listening);
  });
  socket = new WebSocket(endpoint); await once(socket, "open");
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.id) { const request = pending.get(message.id); if (!request) return; pending.delete(message.id); clearTimeout(request.timer); message.error ? request.reject(new CdpProtocolError(request.method, message.error.message)) : request.resolve(message.result); }
    else if (message.method === "Runtime.exceptionThrown") exceptions.push(message.params.exceptionDetails.text + ": " + (message.params.exceptionDetails.exception?.description ?? ""));
  });
  const call = (method, params = {}, scoped = true) => new Promise((resolve, reject) => {
    const id = ++sequence, timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP deadline: ${method}`)); }, 15000);
    pending.set(id, { resolve, reject, timer, method }); socket.send(JSON.stringify({ id, method, params, ...(scoped && session ? { sessionId: session } : {}) }));
  });
  const version = await call("Browser.getVersion", {}, false);
  const target = await call("Target.createTarget", { url: "about:blank" }, false);
  session = (await call("Target.attachToTarget", { targetId: target.targetId, flatten: true }, false)).sessionId;
  await call("Page.enable"); await call("Runtime.enable");
  await call("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme === "auto-light" || theme === "dark" ? "light" : "dark" }, { name: "prefers-reduced-motion", value: "reduce" }] });
  const evaluate = async expression => { const result = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }); if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text); return result.result.value; };
  const wait = expression => waitForReadiness(evaluate, expression, async () =>
    `popup=${JSON.stringify(await evaluate('(()=>{const p=document.querySelector(".erp-search-menu-panel");if(!p)return null;const r=p.getBoundingClientRect();return {left:r.left,right:r.right,viewport:innerWidth,transform:p.style.transform,parent:p.parentElement.getBoundingClientRect().toJSON()};})()'))}`,
    error => error instanceof CdpProtocolError && error.method === "Runtime.evaluate"
      && error.detail === "Inspected target navigated or closed");

  // Actual computed styles on the compiled client, including ancestor backgrounds.
  // These assertions concern readable UI, not product performance ratchets.
  const appearance = async () => {
    const result = await evaluate(`(() => {
      const rgb = value => value.match(/[\\d.]+/g)?.slice(0,3).map(Number) ?? [];
      const luminance = color => rgb(color).map(c=>{const v=c/255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4}).reduce((n,v,i)=>n+v*[.2126,.7152,.0722][i],0);
      const background = node => { for(let n=node;n;n=n.parentElement){const c=getComputedStyle(n).backgroundColor;if(c!=="rgba(0, 0, 0, 0)"&&c!=="transparent")return c;} return getComputedStyle(document.documentElement).backgroundColor; };
      const selectors = ["html", ".app-navigation", ".control-panel", ".content-sheet", ".table-scroll", ".list-table th", ".erp-search-menu-panel", ".settings-sidebar", ".settings-section-title", ".login-card", ".app-bar", ".user-dropdown", ".home-menu-overlay", ".mobile-drawer", ".addons-panel", ".addon-tile", ".addon-info-dialog", ".choice-dialog", ".client-error-dialog", ".wizard-dialog", "input:not([type=checkbox]):not([type=hidden])", "select", "textarea", "button.primary", ".erp-btn-primary"];
      return selectors.flatMap(selector=>[...document.querySelectorAll(selector)].filter(n=>n.getBoundingClientRect().width&&n.getBoundingClientRect().height&&getComputedStyle(n).display!=="none").map(n=>{const bg=background(n),fg=getComputedStyle(n).color,a=luminance(bg),b=luminance(fg);return {selector,bg,fg,luminance:a,contrast:(Math.max(a,b)+.05)/(Math.min(a,b)+.05)};}));
    })()`);
    for (const surface of result) {
      assert.ok(surface.contrast >= 4.5, `readable ${theme} ${surface.selector}: ${JSON.stringify(surface)}`);
      if (surface.selector.includes("primary")) continue;
      if (officeBlue && surface.selector === ".app-navigation" && await evaluate('document.documentElement.dataset.officeTheme === "office-blue"')) assert.ok(surface.luminance < .18, `navy navigation: ${JSON.stringify(surface)}`);
      else if (theme.endsWith("dark")) assert.ok(surface.luminance < .18, `dark surface ${surface.selector}: ${JSON.stringify(surface)}`);
      else assert.ok(surface.luminance > .7, `light surface ${surface.selector}: ${JSON.stringify(surface)}`);
    }
    return result;
  };
  const dialogGeometry = selector => evaluate(`(() => {
    const d=document.querySelector(${JSON.stringify(selector)}),r=d.getBoundingClientRect(),s=getComputedStyle(d);
    let modal; try { modal={matches:d.matches(':modal')}; } catch(error) { modal={error:String(error)}; }
    return {open:d.open,rect:{x:r.x,y:r.y,width:r.width,height:r.height},computed:{position:s.position,top:s.top,right:s.right,bottom:s.bottom,left:s.left,margin:s.margin,transform:s.transform,zIndex:s.zIndex,display:s.display,visibility:s.visibility},modal,viewport:{innerWidth,innerHeight,clientWidth:document.documentElement.clientWidth,clientHeight:document.documentElement.clientHeight},scroll:{x:scrollX,y:scrollY,height:document.scrollingElement.scrollHeight},focus:document.activeElement?.outerHTML.slice(0,300)};
  })()`);
  const assertDialogVisible = layout => {
    const r=layout.rect,v=layout.viewport;
    assert.ok(r.width>0 && r.height>0 && r.x>=0 && r.y>=0 && r.x+r.width<=v.innerWidth+1 && r.y+r.height<=v.innerHeight+1, `dialog lies inside actual viewport: ${JSON.stringify(layout)}`);
  };
  const centerProbeDialog = selector => evaluate(`(() => {const d=document.querySelector(${JSON.stringify(selector)});Object.assign(d.style,{position:'fixed',top:'50%',left:'50%',right:'auto',bottom:'auto',margin:'0',transform:'translate(-50%, -50%)'});})()`);
  const appearanceChecks = [];
  const shot = async (name, width, height, preserveViewport = false) => {
    // A relation scene sets metrics before opening its focus-owned overlay. Reapplying them can alter that scene.
    if (!preserveViewport) await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    else assert.deepEqual(await evaluate('({width:innerWidth,height:innerHeight})'), {width,height}, "preserved capture viewport already matches the requested size");
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
    const image = await call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
    const png = Buffer.from(image.data, "base64");
    const canvas = { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
    assert.deepEqual(canvas, { width, height }, "actual PNG canvas matches the requested capture");
    const viewport = await evaluate('({innerWidth,innerHeight,clientWidth:document.documentElement.clientWidth,clientHeight:document.documentElement.clientHeight,devicePixelRatio})');
    await writeFile(path.join(output, name + ".png"), png); screenshots.push({ name: name + ".png", ...canvas, viewport });
    appearanceChecks.push({ name, surfaces: await appearance() });
  };
  if (officeBlue) {
    await call("Page.navigate", { url: base + "/erp/action-20?view_type=list" });
    await wait('document.documentElement.dataset.officeTheme === "office-blue" && document.querySelector(".list-table") !== null');
    assert.equal(await evaluate('document.querySelector(".app-bar .office-header-search input") !== null'), true, "resolved database theme mounts the functioning header search");
    assert.equal(await evaluate('document.querySelector(".app-bar .user-company")?.textContent'), "Rendering fixture company", "resolved theme uses actual company identity");
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    assert.equal(await evaluate('document.querySelector("[data-control-categories] .list-state-tabs") !== null'), true, "category placement belongs to the extensible XML template");
    assert.equal(await evaluate('document.querySelector(".list-page-size").value'), "80", "declared default is selected");
    for (const [width, height] of [[1440, 900], [768, 1024], [375, 812]]) {
      await shot(`office-blue-list-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `list fits ${width}`);
      const listLayout = await evaluate(`(() => {
        const bounds = selector => { const r=document.querySelector(selector).getBoundingClientRect(); return {x:r.x,y:r.y,width:r.width,height:r.height,right:r.right,bottom:r.bottom}; };
        return { title:bounds('.control-panel-list h1'), search:bounds('.control-panel-list [data-control-search]'), actions:bounds('.control-panel-list [data-control-actions]'), table:bounds('.list-results'), footer:bounds('.list-footer'), category:bounds('.list-state-tabs'), header:bounds('.list-table thead tr'), row:bounds('.list-table tbody tr'), checkbox:bounds('.list-table input[type=checkbox]'), current:document.querySelector('.list-footer [aria-current="page"]')?.textContent };
      })()`);
      listLayouts.push({width, height, ...listLayout});
      assert.ok(listLayout.category.bottom <= listLayout.search.y, `search below category ${width}`);
      assert.ok(listLayout.title.bottom <= listLayout.search.y, `list search below title ${width}`);
      assert.ok(listLayout.search.bottom <= listLayout.table.y, `list table below search ${width}`);
      assert.ok(listLayout.table.bottom <= listLayout.footer.y, `list paging below table ${width}`);
      assert.equal(listLayout.current, "1", `actual current page ${width}`);
      if (width === 1440) assert.ok(listLayout.actions.x > listLayout.title.right, "list actions to right of title");
      if (width === 1440) {
        assert.equal(await evaluate('getComputedStyle(document.querySelector(".office-navigation")).display'), "flex");
        assert.equal(await evaluate('getComputedStyle(document.querySelector(".app-navigation")).backgroundColor'), "rgb(14, 30, 50)");
      } else {
        await evaluate('document.querySelector("[data-burger-menu]").click()');
        await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
        assert.equal(await evaluate('document.activeElement.classList.contains("drawer-close-btn")'), true);
        await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
        await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === true');
      }
    }
    await evaluate('document.querySelector(".search-menu-toggle").click()');
    await wait('document.querySelector(".erp-search-menu-panel").style.display !== "none"');
    assert.equal(await evaluate('document.activeElement.closest(".erp-search-menu-panel") !== null'), true, "separate More filters control opens the existing accessible menu");
    await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
    await wait('document.querySelector(".erp-search-menu-panel").style.display === "none"');
    await evaluate(`document.querySelector('[data-tab-token="value:high"]').click()`);
    await wait('document.querySelector(".list-results .empty-state") !== null');
    assert.deepEqual(pageReads.at(-1).domain, [["priority", "=", "high"]]);
    assert.deepEqual(groupDomains.at(-1), [], "counts exclude the category's own filter");
    assert.equal(await evaluate(`document.querySelector('[data-tab-token="all"] .list-tab-count').textContent`), "2");
    await evaluate(`document.querySelector('[data-tab-token="all"]').click()`);
    await wait('document.querySelector(".list-table tbody tr") !== null');
    await evaluate('const size=document.querySelector(".list-page-size"); size.value="50"; size.dispatchEvent(new Event("change"));');
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    assert.equal(pageReads.at(-1).limit, 50); assert.equal(pageReads.at(-1).offset, 0);
    rejectNextCount = true;
    await evaluate(`document.querySelector('[data-tab-token="value:normal"]').click()`);
    await wait('document.querySelector(".list-load-error") !== null && document.querySelector(".client-error-dialog")?.open === true');
    assert.equal(await evaluate('document.querySelector(".pager-count").textContent'), "Results unavailable");
    assert.equal(await evaluate('document.querySelector(".pager-prev").disabled && document.querySelector(".pager-next").disabled && !document.querySelector(".pager-pages [aria-current]")'), true);
    errorLayouts.push({phase:"before_capture", ...await evaluate(`(() => { const d=document.querySelector('.client-error-dialog'),r=d.getBoundingClientRect(),s=getComputedStyle(d); return {open:d.open,x:r.x,y:r.y,width:r.width,height:r.height,position:s.position,top:s.top,visibility:s.visibility}; })()`)});
    assert.equal(await evaluate('document.querySelector(".list-table tbody tr") === null'), true, "failed count cannot leave stale records");
    assert.equal(await evaluate(`document.querySelector('[data-tab-token="all"] .list-tab-count').textContent`), "—");
    await shot("office-blue-list-count-failure", 1440, 900);
    errorLayouts.push({phase:"after_capture", ...await evaluate(`(() => { const d=document.querySelector('.client-error-dialog'),r=d.getBoundingClientRect(),s=getComputedStyle(d); return {open:d.open,x:r.x,y:r.y,width:r.width,height:r.height,position:s.position,top:s.top,visibility:s.visibility}; })()`)});
    if (!dialogProbe) assertDialogVisible(await dialogGeometry('.client-error-dialog'));
    if (dialogProbe) {
      errorLayouts.push({phase:"error_original_computed", ...await dialogGeometry('.client-error-dialog')});
      await centerProbeDialog('.client-error-dialog');
      await shot("diagnostic-error-explicit-center",1440,900);
      errorLayouts.push({phase:"error_centered_probe", ...await dialogGeometry('.client-error-dialog')});
      await evaluate('document.querySelector("[data-error-close]").click()');
      await wait('document.querySelector(".client-error-dialog") === null');
      await evaluate(`(() => {const d=document.createElement('dialog');d.id='native-dialog-probe';d.textContent='Isolated native dialog geometry probe';document.body.append(d);d.showModal();})()`);
      await shot("diagnostic-native-dialog-unstyled",1440,900);
      errorLayouts.push({phase:"native_unstyled", ...await dialogGeometry('#native-dialog-probe')});
      await centerProbeDialog('#native-dialog-probe');
      await shot("diagnostic-native-dialog-centered",1440,900);
      errorLayouts.push({phase:"native_centered_probe", ...await dialogGeometry('#native-dialog-probe')});
      await evaluate(`(() => {const d=document.querySelector('#native-dialog-probe');d.close();d.remove();})()`);
    }
    await call("Page.navigate", { url: base + "/erp/action-20?view_type=list" });
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false" && document.querySelector(".list-table tbody tr") !== null');
    {
      await evaluate('document.querySelector(".list-table tbody input[type=checkbox]").click();document.querySelector(".action-toggle-btn").click();document.querySelector(".action-delete-btn").click();');
      await wait('document.querySelector(".choice-dialog")?.open === true');
      await shot(dialogProbe ? "diagnostic-real-choice-original" : "office-blue-delete-confirmation",1440,900);
      const choiceLayout=await dialogGeometry('.choice-dialog');
      errorLayouts.push({phase:"real_choice_original", ...choiceLayout});
      if (!dialogProbe) {
        assertDialogVisible(choiceLayout);
        await shot("office-blue-delete-confirmation-narrow",375,812);
        assertDialogVisible(await dialogGeometry('.choice-dialog'));
      }
      if (dialogProbe) {
      await centerProbeDialog('.choice-dialog');
      await shot("diagnostic-real-choice-centered",1440,900);
      errorLayouts.push({phase:"real_choice_centered_probe", ...await dialogGeometry('.choice-dialog')});
      await writeFile(path.join(output,'dialog-probe.json'),JSON.stringify({status:'DIAGNOSTIC_NOT_ACCEPTANCE',source_revision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),time_utc:new Date().toISOString(),hostname:hostname(),browser:version,layouts:errorLayouts},null,2)+'\n');
      }
      await evaluate('[...document.querySelectorAll(".choice-dialog button")].find(b=>b.textContent.trim()==="Cancel").click()');
      await wait('document.querySelector(".choice-dialog") === null');
    }
    if (!listOnly) {
    await evaluate('document.querySelector(".office-navigation [aria-current=page]").closest("details").open = false');
    await evaluate('document.querySelector("[data-burger-menu]").click()');
    await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
    // A rendered frame is required before reading geometry after a metrics override.
    // Moli can deliver resize/media events before its last painted layout has been refreshed.
    await shot("office-blue-resize-desktop", 1440, 900);
    await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === true');
    assert.equal(await evaluate(`(() => { const active=document.activeElement; return active.matches('.office-navigation [aria-current="page"],button.brand') && active.getBoundingClientRect().width>0 && getComputedStyle(active).visibility!=="hidden" && !active.closest("[inert]"); })()`), true, "resize restores visible Office Blue focus");
    assert.equal(await evaluate('document.body.style.overflow'), "");
    await call("Page.navigate", { url: base + "/erp/action-20?id=5&view_type=form" });
    await wait('document.querySelector("input[name=name]") !== null');
    assert.ok(await evaluate('parseFloat(getComputedStyle(document.querySelector(".form-breadcrumb-record")).fontSize) <= 13'), "breadcrumb remains secondary to the sheet title");
    assert.equal(await evaluate('document.querySelectorAll(".content-sheet .record-form input[name=name]").length === 1 && document.querySelector(".control-panel input[name=name]") === null'), true, "one editable record title stays inside the declared form sheet");
    assert.equal(await evaluate('document.querySelector(".control-panel .form-record-actions .form-header-btn") !== null && document.querySelector(".content-sheet .form-header") === null'), true, "real declared object actions live with record commands");
    assert.equal(await evaluate('document.querySelector(".form-record-tabs [role=tablist]") !== null && document.querySelector(".record-form [role=tablist]") === null'), true, "sheet notebook navigation sits above declared content");
    for (const [width, height] of [[1536, 1024], [1440, 900], [768, 1024], [375, 812]]) {
      await shot(`office-blue-form-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `form fits ${width}`);
      const aside = await evaluate(`(() => { const side=document.querySelector('.record-form-aside').getBoundingClientRect(),sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(); return {side:side.toJSON(),sheet:sheet.toJSON()}; })()`);
      assert.ok(width > 1120 ? aside.side.left >= aside.sheet.right : aside.side.top >= aside.sheet.bottom, `real record information rail has responsive placement ${width}`);
      const smart = await evaluate(`(() => { const box=document.querySelector('.form-button-box').getBoundingClientRect(),sheet=document.querySelector('.form-sheet-layout').getBoundingClientRect(),button=document.querySelector('.form-stat-button');return {box:box.toJSON(),sheet:sheet.toJSON(),name:button.getAttribute('aria-label'),nested:button.querySelectorAll('input,select,textarea,button').length}; })()`);
      assert.ok(Math.abs(smart.box.left-smart.sheet.left)<2 && Math.abs(smart.box.right-smart.sheet.right)<2, `declared smart box spans sheet ${width}: ${JSON.stringify(smart)}`);
      assert.equal(smart.name, "17 Lines"); assert.equal(smart.nested, 0, "smart statistic is not an editable nested control");
      const geometry = await evaluate(`(() => {
        const field = document.querySelector('select[name="priority"]').closest('.field-label');
        const label = field.querySelector('span').getBoundingClientRect();
        const control = field.querySelector('select').getBoundingClientRect();
        const section = field.closest('.form-group');
        const title = document.querySelector('.record-form input[name="name"]');
        const heading = title.closest('.form-title-block,.form-title-field').getBoundingClientRect();
        const actions = document.querySelector('[data-control-actions]').getBoundingClientRect();
        return { labelBottom: label.bottom, controlTop: control.top, controlHeight: control.height,
          sectionBorder: getComputedStyle(section).borderTopWidth, headingLeft: heading.left,
          headingTop: heading.top, actionsBottom: actions.bottom, headingInSheet: title.closest(".content-sheet .record-form") !== null,
          actionsLeft: actions.left, tabBackground: getComputedStyle(document.querySelector('[role="tab"][aria-selected="true"]')).backgroundColor };
      })()`);
      assert.ok(geometry.labelBottom <= geometry.controlTop, `field label sits above its control at ${width}: ${JSON.stringify(geometry)}`);
      assert.notEqual(geometry.sectionBorder, "0px", "declared field section has a visible boundary");
      if (width === 1440) {
        assert.equal(geometry.headingInSheet, true, "record title remains in its form sheet");
        assert.ok(geometry.actionsBottom <= geometry.headingTop, "header commands remain above the sheet title");
        assert.ok(geometry.controlHeight >= 32 && geometry.controlHeight <= 36, `compact desktop controls: ${JSON.stringify(geometry)}`);
      } else if (width <= 640) assert.ok(geometry.controlHeight >= 40, "phone controls preserve touch target height");
      assert.equal(geometry.tabBackground, "rgba(0, 0, 0, 0)", "selected notebook uses a quiet underline without a filled button");
      await evaluate(`(() => { const title=document.querySelector('input[name="name"]'); title.value += ' edited'; title.dispatchEvent(new Event('change')); })()`);
      await wait('document.querySelector(".status-actions").style.display !== "none"');
      await shot(`office-blue-form-dirty-${width}`, width, height);
      const commands = await evaluate(`([...document.querySelectorAll('.status-btn-save,.status-btn-discard,.form-header-btn,[data-control-actions] > button,.action-toggle-btn,.form-pager,.view-switcher')]
        .map(node=>({label:node.getAttribute('aria-label')||node.textContent,rect:node.getBoundingClientRect().toJSON()})).filter(item=>item.rect.width && item.rect.height))`);
      for (const [index, command] of commands.entries()) {
        assert.ok(command.rect.left >= 0 && command.rect.right <= width, `dirty command fits ${width}: ${JSON.stringify(command)}`);
        for (const other of commands.slice(index + 1)) {
          const overlapX = Math.min(command.rect.right, other.rect.right) - Math.max(command.rect.left, other.rect.left);
          const overlapY = Math.min(command.rect.bottom, other.rect.bottom) - Math.max(command.rect.top, other.rect.top);
          assert.ok(overlapX <= 0 || overlapY <= 0, `dirty commands do not overlap at ${width}: ${JSON.stringify([command, other])}`);
        }
      }
      await evaluate(`(() => { const title=document.querySelector('input[name="name"]'); title.value=''; document.querySelector('.status-btn-save').click(); })()`);
      await wait('document.querySelector(".record-form input[name=name][aria-invalid=true]") !== null');
      assert.equal(writes.length, 0, "invalid sheet title never reaches web_save");
      await shot(`office-blue-form-invalid-${width}`, width, height);
      assert.equal(await evaluate('getComputedStyle(document.querySelector("select[name=priority]")).backgroundImage !== "none"'), true, "selection keeps its dropdown chevron");
      assert.equal(await evaluate('document.querySelector("input[name=reference]").disabled'), true, "readonly reference stays readonly");
      assert.equal(await evaluate('document.querySelector("input[name=scheduled_date]").closest(".field-input-icon").querySelector("[data-icon=calendar]") !== null'), true, "date keeps native input and decorative calendar");
      assert.equal(await evaluate(`(() => { const input=document.querySelector('input[name=scheduled_date]'); const calendar=input.closest('.field-input-icon').querySelector('[data-icon=calendar]').getBoundingClientRect(); return calendar.right <= input.getBoundingClientRect().left + parseFloat(getComputedStyle(input).paddingLeft); })()`), true, "calendar icon stays inside reserved space before the date text");
      await evaluate('document.querySelector(".status-btn-discard").click()');
      await wait('document.querySelector(".status-actions").style.display === "none"');
    }
    await evaluate('document.querySelector(".form-stat-button").focus()');
    assert.equal(await evaluate('document.activeElement.classList.contains("form-stat-button")'),true,"smart action receives keyboard focus");
    await evaluate('document.querySelector(".form-stat-button").click()');
    await wait('document.querySelector(".form-stat-button")?.getAttribute("aria-label") === "18 Lines"');
    assert.equal(inspectCalls.length,1,"smart action dispatches the declared record method once and refreshes its actual fixture count");
    const changesBeforeStatus = calls.filter(path => path.endsWith("/onchange")).length;
    await evaluate(`document.querySelector('.form-statusbar [data-value="high"]').click(); document.querySelector('.form-statusbar [data-value="high"]').dispatchEvent(new Event('click'));`);
    assert.equal(await evaluate('document.querySelector(".form-statusbar [aria-current=step]").textContent'), "Normal", "default statusbar cannot mutate the record");
    assert.equal(calls.filter(path => path.endsWith("/onchange")).length, changesBeforeStatus, "statusbar sends no change RPC");
    await evaluate('document.querySelector(".form-header-btn").click()');
    await wait('document.querySelector(".choice-dialog")?.open === true');
    await evaluate('[...document.querySelectorAll(".choice-dialog button")].find(button => button.textContent === "Lend").click()');
    await wait('document.querySelector(".form-statusbar [aria-current=step]")?.textContent === "High"');
    assert.equal(lendCalls.length, 1, "the declared object action remains the state transition path");
    await evaluate('document.querySelector("[role=tab]").focus()');
    assert.equal(await evaluate('document.activeElement.getAttribute("role")'), "tab");
    await call("Page.navigate", { url: base + "/erp/action-21" });
    await wait('document.querySelector(".settings-form") !== null');
    await evaluate(`document.querySelector('[data-app="theme_office_blue"]').click()`);
    await evaluate(`const choice=document.querySelector('select[name="theme_office_blue.appearance"]'); choice.value="default"; choice.dispatchEvent(new Event("change"));`);
    await evaluate('[...document.querySelectorAll("button")].find(b=>b.textContent==="Save").click()');
    await wait('document.querySelector("[data-client-notice]").textContent.includes("saved")');
    await call("Page.reload");
    await wait('document.querySelector(".settings-form") !== null');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    assert.equal(await evaluate('document.querySelector("#database-appearance-stylesheet") !== null'), false);
    officeInstalled = false;
    await call("Page.navigate", { url: base + "/erp" });
    await wait('document.querySelector(".home-grid") !== null');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    officeInstalled = true; officeChoice = "office-blue"; missingOfficeCss = true;
    await call("Page.navigate", { url: base + "/erp" });
    await wait('document.querySelector("#client-error-message")?.textContent.includes("Office Blue stylesheet could not be loaded")');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    await shot("office-blue-missing-stylesheet", 1440, 900);
    }
  } else {
  await call("Page.navigate", { url: base + "/erp/login" }); await wait('document.querySelector("form[action=\\"/erp/login\\"]") !== null');
  assert.equal(await evaluate(`[...document.querySelectorAll("img")].every(img=>["/erp/brand/logo.svg","/erp/brand/logo-dark.svg"].includes(img.getAttribute("src")))`), true, "only the pinned brand logos render as images");
  assert.equal(await evaluate('document.querySelector("input[name=password]").autocomplete'), "current-password");
  assert.equal(await evaluate('document.querySelector("input[name=csrf_token]").value'), "fixture-not-a-real-token");
  // Native autofocus runs after input connection; retain the exact expected-focus assertion.
  await wait('document.activeElement.id === "login-name"');
  assert.equal(await evaluate('document.activeElement.id'),"login-name","the login error view keeps keyboard focus on the login input");
  assert.equal(await evaluate('document.querySelector(".login-error")?.getAttribute("role")'),"alert","login failure is announced");
  await shot("fixture-login-desktop", 1440, 900); await shot("fixture-login-mobile", 390, 844);
  await call("Page.navigate", { url: base + "/erp" });
  await wait('document.querySelector(".home-grid") !== null');
  await shot("fixture-home-menu-desktop", 1440, 900); await shot("fixture-home-menu-mobile", 390, 844);
  assert.equal(await evaluate(`(()=>{const visible=[...document.querySelectorAll('.app-tile')].filter(n=>n.getBoundingClientRect().width&&n.getBoundingClientRect().height&&!n.closest('[inert]'));const names=visible.map(n=>n.textContent);return visible.length===4&&new Set(names).size===4;})()`), true, "one accessible launcher per app across the complete mounted shell");
  await evaluate('document.querySelector("[data-burger-menu]").click()');
  await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
  assert.equal(await evaluate('document.activeElement.classList.contains("drawer-close-btn")'), true);
  await shot("fixture-navigation-drawer-mobile",390,844);
  await evaluate('document.querySelector(".drawer-close-btn").click()');
  assert.equal(await evaluate('document.activeElement.hasAttribute("data-burger-menu")'), true);
  await evaluate('document.querySelector(".user-button").click()');
  await wait('document.querySelector(".user-dropdown")?.classList.contains("open") === true');
  await shot("fixture-user-menu-mobile",390,844);
  await evaluate('document.querySelector(".user-button").click()');
  assert.equal(await evaluate(`[...document.querySelectorAll('.home-grid .app-tile')].every(tile=>{const svg=tile.querySelector('.app-icon svg');return svg?.getAttribute('aria-hidden')==='true'&&svg.getAttribute('focusable')==='false'&&svg.getBoundingClientRect().width>0&&tile.querySelector('.app-caption').textContent.length>0})`), true, "visible decorative launcher SVGs and captions");
  assert.equal(await evaluate(`document.querySelector('.home-grid [data-icon="notebook-pen"]')!==null&&document.querySelector('.home-grid [data-icon="settings"]')!==null`), true, "Notes and Settings use pinned icons");
  const homeAlignment=await evaluate(`(()=>{const grid=document.querySelector('.home-grid').getBoundingClientRect(),tiles=[...document.querySelectorAll('.app-tile')].map(e=>e.getBoundingClientRect());return {grid: {x:grid.x,width:grid.width}, left:Math.min(...tiles.map(r=>r.left)),right:Math.max(...tiles.map(r=>r.right))};})()`);
  assert.ok(Math.abs((homeAlignment.left+homeAlignment.right)/2-(homeAlignment.grid.x+homeAlignment.grid.width/2))<2,"launcher row centered");
  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Page.navigate", {url:base+"/erp/action-20?view_type=list"});
  await wait('document.querySelectorAll(".list-table tbody tr").length === 2');
  assert.equal(await evaluate('document.querySelectorAll(".list-table tbody tr").length'),2,"selection fixture provides two records");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await call("Emulation.setDeviceMetricsOverride", {width,height,deviceScaleFactor:1,mobile:!viewportOnly&&width<700});
    await call("Page.navigate", {url:base+"/erp/action-20?view_type=list"});
    await wait('document.querySelectorAll(".list-table tbody tr").length === 2');
    await evaluate('[...document.querySelectorAll(".list-row-select")].forEach(input=>input.click())');
    await wait('document.querySelector(".selection-count")?.textContent === "2 selected"');
    assert.equal(await evaluate('document.querySelectorAll(".list-table tbody tr.row-selected").length'),2,"selected row tint matches the selected count");
    await evaluate('document.querySelector(".action-toggle-btn").click()');
    await wait('document.querySelector(".action-menu")?.classList.contains("open")');
    assert.equal(await evaluate('Boolean(document.querySelector(".action-delete-btn"))'),true,"selected-list actions are available");
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`selected list does not overflow at ${width}px`);
    await shot(`fixture-list-selection-${width}`,width,height);
  }
  await call("Page.navigate", { url: base + "/erp/action-20?id=5&view_type=form" });
  await wait('document.querySelector(".acceptance-note-title input") !== null && document.querySelector(".acceptance-note-guide") !== null');
  assert.equal(await evaluate('document.querySelector(".acceptance-note-title input").value'), note);
  assert.equal(await evaluate(`document.querySelector('[aria-current="page"]')?.textContent`), "Notes");
  await shot("fixture-form-groups-desktop",1440,900);
  const geometry=await evaluate(`(()=>{
   const groups=[...document.querySelectorAll('.record-form .form-group-title')].map(e=>e.parentElement.getBoundingClientRect());
   const title=document.querySelector('.form-title-block,.form-title-field').getBoundingClientRect();
   const sheet=document.querySelector('.content-sheet').getBoundingClientRect();
   const required=[...document.querySelectorAll('.field-required > span')].map(e=>({text:e.textContent,after:getComputedStyle(e,'::after').content}));
   return {groups:groups.map(r=>({x:r.x,y:r.y,width:r.width})),title:{width:title.width},sheet:{width:sheet.width},required};
  })()`);
  assert.equal(geometry.groups.length,2);
  assert.ok(geometry.groups[0].x < geometry.groups[1].x && Math.abs(geometry.groups[0].y-geometry.groups[1].y)<2,"declared groups share desktop row");
  assert.ok(geometry.title.width>geometry.sheet.width*.8,"title spans the sheet");
  assert.ok(geometry.required.every(x=>x.text.split('*').length===2 && ["none","normal"].includes(x.after)),"one required marker per field");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await shot(`default-form-rail-${width}`,width,height);
    const layout=await evaluate(`(() => { const side=document.querySelector('.record-form-aside').getBoundingClientRect(), sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(); return {side:side.toJSON(),sheet:sheet.toJSON(),overflow:document.documentElement.scrollWidth>innerWidth}; })()`);
    assert.ok(width>1120 ? layout.side.left>=layout.sheet.right : layout.side.top>=layout.sheet.bottom, `declared aside placement at ${width}`);
    assert.equal(layout.overflow,false,`form fits ${width}`);
    // shares the side column under the aside, elsewhere it follows the sheet. It adds no overflow.
    const chatter=await evaluate(`(() => { const slot=document.querySelector('.record-form-viewport > .record-form-chatter'), sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(), side=document.querySelector('.record-form-aside').getBoundingClientRect(); if(!slot) return null; const r=slot.getBoundingClientRect(); return {rect:r.toJSON(),sheet:sheet.toJSON(),side:side.toJSON(),data:{...slot.firstElementChild.dataset},label:slot.getAttribute('aria-label'),count:document.querySelectorAll('.record-form-chatter').length}; })()`);
    assert.ok(chatter,`chatter host rendered at ${width}`);
    assert.equal(chatter.count,1,"one chatter host");
    assert.equal(chatter.label,"Chatter");
    assert.deepEqual(chatter.data,{reloadOnPost:"true",reloadOnAttachment:"false",reloadOnActivity:"false",reloadOnFollower:"false",openAttachments:"false",hasActivities:"true"});
    assert.ok(chatter.rect.top>=chatter.side.bottom-1 || chatter.rect.left>=chatter.sheet.right,`chatter does not overlap the aside at ${width}`);
    // Chatter panel layout (any theme): own column of 360-440px on wide viewports, under the sheet and never wider than it below.
    if (width>1120) { assert.ok(chatter.rect.left>=chatter.sheet.right && chatter.rect.top>=chatter.side.bottom-1,`chatter sits under the aside in the side column at ${width}`); assert.ok(chatter.rect.width>=360 && chatter.rect.width<=440,`chatter column width at ${width}: ${chatter.rect.width}`); }
    else { assert.ok(chatter.rect.top>=chatter.sheet.bottom-1,`chatter follows the sheet at ${width}`); assert.ok(chatter.rect.width<=chatter.sheet.width+1,`chatter is never wider than the sheet at ${width}`); }
    await wait('document.querySelectorAll(".record-form-chatter .erp-mail-message").length === 3');
    assert.equal(await evaluate('Boolean(document.querySelector(".record-form-chatter .erp-mail-chatter-header .erp-mail-kinds"))'),true,`chatter header carries the post-type switch at ${width}`);
    assert.equal(await evaluate('document.querySelector(".record-form-chatter .erp-chatter-composer").classList.contains("is-collapsed")'),true,"composer starts collapsed");
    await shot(`chatter-${width}`,width,height);
  }
  assert.equal(await evaluate('document.querySelector(".form-breadcrumb-path [aria-current=page]")?.textContent'), note);
  assert.equal(await evaluate('document.querySelector(".control-panel .form-statusbar [aria-current=step]")?.textContent'), "Normal");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await call("Emulation.setDeviceMetricsOverride",{width,height,deviceScaleFactor:1,mobile:!viewportOnly&&width<700});
    await evaluate('(()=>{const input=document.querySelector("input[name=owner_id]");input.focus();input.value="Rendering";input.dispatchEvent(new Event("input",{bubbles:true}));})()');
    await wait('document.querySelector(".editor-autocomplete-list")?.textContent.includes("Rendering fixture user")');
    assert.ok(await evaluate('document.querySelectorAll(".editor-autocomplete-list [role=option]").length >= 1'),`relation suggestions render at ${width}px`);
    const menuBounds=await evaluate('(()=>{const r=document.querySelector(".editor-autocomplete-list").getBoundingClientRect();return {left:r.left,right:r.right,width:innerWidth};})()');
    assert.ok(menuBounds.left>=0&&menuBounds.right<=menuBounds.width,`relation dropdown fits ${width}px: ${JSON.stringify(menuBounds)}`);
    await shot(`fixture-relation-dropdown-${width}`,width,height,true);
    assert.ok(await evaluate('document.querySelectorAll(".editor-autocomplete-list [role=option]").length >= 1 && document.querySelector("input[name=owner_id]").getAttribute("aria-expanded") === "true"'), `relation scene remains open after capture at ${width}px`);
    await evaluate('document.querySelector("input[name=owner_id]").dispatchEvent(new KeyboardEvent("keydown",{key:"Escape",bubbles:true}))');
  }
  // Header object button, driven by the real keyboard: Enter opens the confirmation, Space accepts it.
  assert.equal(await evaluate(`document.querySelector('.form-header-actions')?.getAttribute('role')`), "toolbar");
  assert.deepEqual(await evaluate(`[...document.querySelectorAll('.form-header-actions .form-header-btn')].map(b=>b.textContent)`), ["Lend"]);
  assert.equal(await evaluate(`Boolean(document.querySelector('.control-panel .form-header'))`), true, "header is separate from the sheet");
  await shot("fixture-form-header-desktop", 1440, 900);
  const press = async (key, code, keyCode, text) => {
    for (const type of ["keyDown", "keyUp"]) await call("Input.dispatchKeyEvent", { type, key, code, windowsVirtualKeyCode: keyCode, ...(type === "keyDown" ? { text } : {}) });
  };
  await evaluate(`document.querySelector('.form-header-btn').focus()`);
  await press("Enter", "Enter", 13, "\r");
  await wait(`document.querySelector('dialog')?.textContent.includes('Lend this book?')`);
  await shot("fixture-confirmation-desktop",1440,900);
  assert.equal(lendCalls.length, 0, "the method waits for the confirmation");
  assert.equal(await evaluate(`document.activeElement.closest('dialog') !== null`), true, "focus moves into the confirmation dialog");
  await press(" ", "Space", 32, " ");
  await wait(`document.querySelector('.form-header-actions .form-header-btn') === null`);
  assert.equal(lendCalls.length, 1); assert.deepEqual(lendCalls[0].ids, [5]);
  assert.equal(await evaluate(`document.querySelector('.form-statusbar [aria-current=step]')?.textContent`), "High", "declared status remains visible when the action disappears");
  priorityValue = "normal";
  await evaluate(`document.querySelector('[role=tab]:last-child').click()`);
  assert.equal(await evaluate(`document.querySelector('[role=tab][aria-selected=true]').textContent`),"Internal Memo");
  const callsBeforeTabs=calls.length;
  await evaluate(`document.querySelector('[role=tab][aria-selected=true]').dispatchEvent(new KeyboardEvent('keydown',{key:'Home',bubbles:true}))`);
  assert.equal(calls.length,callsBeforeTabs,"tab switch does not issue RPC or writes");
  const writesBeforeInvalid=writes.length;
  await evaluate(`(()=>{const input=document.querySelector('textarea[name=internal_memo]');input.required=true;document.querySelector('.record-form').requestSubmit();})()`);
  await wait(`document.querySelector('[role=tab][aria-selected=true]')?.textContent==='Internal Memo'`);
  assert.equal(writes.length,writesBeforeInvalid,"invalid hidden-page field must block save even from Enter/requestSubmit");
  await evaluate(`(()=>{document.querySelector('textarea[name=internal_memo]').required=false;document.querySelector('[role=tab]').click();})()`);
  for(const width of [800,390]) {
   await shot(`fixture-form-groups-${width}`,width,900);
   assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`form should fit ${width}px`);
  }
  await shot("fixture-form-groups-desktop",1440,900);
  await shot("fixture-route-form-desktop", 1440, 900);
  await shot("fixture-route-form-mobile", 375, 812);
  await evaluate('document.querySelector("[data-skip-content]").click()');
  assert.equal(await evaluate('document.activeElement.id'), "erp-content"); assert.equal(await evaluate('location.hash'), "");
  await shot("fixture-addon-form-desktop", 1440, 900); await shot("fixture-addon-form-mobile", 390, 844);
  await evaluate('(()=>{ const input=document.querySelector(".acceptance-note-title input"); input.value="Compiled addon event"; input.dispatchEvent(new Event("change",{bubbles:true})); })()');
  await wait('document.querySelector(".acceptance-note-title input")?.value === "Compiled addon event" && document.querySelector(".acceptance-note-title input")?.disabled === false');
  await wait('document.querySelector(".status-badge.dirty")?.textContent === "Unsaved changes"');
  assert.equal(await evaluate('document.querySelector(".status-badge.dirty").hidden'),false,"dirty state is visibly marked");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await shot(`fixture-form-dirty-${width}`,width,height);
    if (width < 700) {
      assert.equal(await evaluate('[...document.querySelectorAll(".status-actions button")].filter(button=>button.getClientRects().length).length'),2,"phone exposes save and discard actions for the dirty form");
    } else {
      assert.equal(await evaluate('document.querySelector(".status-badge.dirty")?.getClientRects().length > 0'),true,`dirty status badge remains visible at ${width}px`);
    }
  }
  await call("Emulation.setDeviceMetricsOverride",{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await evaluate('[...document.querySelectorAll("button")].find(button=>button.textContent==="Save").click()');
  await wait('[...document.querySelectorAll("[data-client-notice] .client-notification-message")].some(node => node.textContent === "Record saved.")');
  assert.equal(writes.length, 1); assert.equal(writes[0].vals.name, "Compiled addon event");
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Notes").click()');
  await wait('document.querySelector("table[aria-label=Records]") !== null');
  await shot("fixture-list-desktop", 1440, 900);
  const toolbarLayout = await evaluate('(()=>{const rect=s=>{const r=document.querySelector(s).getBoundingClientRect();return {left:r.left,right:r.right,y:r.y+r.height/2};};return {action:rect("[data-control-actions] button"),title:rect(".breadcrumbs"),search:rect("[data-control-search]"),tools:rect("[data-control-tools]")};})()');
  assert.ok(toolbarLayout.action.right <= toolbarLayout.title.left, "primary action must precede title");
  assert.ok(toolbarLayout.title.right <= toolbarLayout.search.left + 1, "title must not overlap search");
  assert.ok(Math.abs(toolbarLayout.action.y - toolbarLayout.search.y) < 10, "desktop actions and search must share one row");

  const shortNote = note;
  note = "Long record title " + "department and purchasing operations ".repeat(30);
  for (const [width, height] of [[1440, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    await evaluate('document.documentElement.dataset.reloadProbe = "pending"');
    await call("Page.reload");
    await wait('document.documentElement !== null && document.documentElement.dataset.reloadProbe !== "pending" && document.querySelector("table tbody td button.link")?.textContent.startsWith("Long record title") && getComputedStyle(document.querySelector("table[aria-label=Records]")).tableLayout === "fixed"');
    // First frame after reload: table columns must have settled synchronously upon DOM attachment
    // without waiting for requestAnimationFrame.
    const firstFrame = await evaluate('(()=>{const table=document.querySelector("table[aria-label=Records]");return {layout:getComputedStyle(table).tableLayout,autoLayoutClass:table.classList.contains("list-auto-layout"),cols:[...table.querySelectorAll("col")].map(c=>c.style.width)};})()');
    assert.equal(firstFrame.layout, "fixed", `table must settle to fixed layout synchronously on reload without waiting for requestAnimationFrame: ${JSON.stringify(firstFrame)}`);
    assert.equal(firstFrame.autoLayoutClass, false, `table must not retain list-auto-layout class on reload: ${JSON.stringify(firstFrame)}`);
    assert.ok(firstFrame.cols.length > 1 && firstFrame.cols.slice(1).every(w => /^[0-9]+px$/.test(w)), `columns must have settled inline pixel widths on first frame: ${JSON.stringify(firstFrame)}`);
    const longCell = await evaluate('(()=>{const cell=document.querySelector("table tbody td:not(.list-record-selector)"),link=cell.querySelector("button"),table=document.querySelector("table");return {page:document.documentElement.scrollWidth,viewport:innerWidth,table:table.getBoundingClientRect().width,cell:cell.getBoundingClientRect().width,link:link.getBoundingClientRect().width,full:cell.title===link.textContent,clipped:link.scrollWidth>link.clientWidth,layout:getComputedStyle(table).tableLayout};})()');
    assert.ok(longCell.page <= width && longCell.table <= width && longCell.link <= longCell.cell && longCell.clipped && longCell.full, JSON.stringify(longCell));
    assert.equal(longCell.layout, "fixed");
    await shot(`fixture-list-long-${width}`, width, height);
  }
  note = shortNote;
  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Page.reload"); await wait('document.querySelector("table tbody td button.link")?.textContent === "Compiled addon event"');

  // Synchronize real rendering after reload before native pointer hit testing (Moli publishes layout on capture).
  await shot("fixture-list-before-resize", 1440, 900);
  // Exercise actual CDP pointer capture; a synthetic DOM event alone cannot prove dragging.
  const resize = await evaluate('(()=>{const h=document.querySelector(".column-resize-handle"),r=h.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2,width:h.parentElement.getBoundingClientRect().width};})()');
  assert.ok(resize.width > 0 && resize.x > 0 && resize.y > 0, `resize handle has actual rendered geometry: ${JSON.stringify(resize)}`);
  await call("Input.dispatchMouseEvent", {type:"mousePressed",x:resize.x,y:resize.y,button:"left",clickCount:1});
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:resize.x+60,y:resize.y,button:"left",buttons:1});
  await call("Input.dispatchMouseEvent", {type:"mouseReleased",x:resize.x+60,y:resize.y,button:"left",clickCount:1});
  await shot("fixture-list-after-resize", 1440, 900);
  const resized = await evaluate('document.querySelector(".column-resize-handle").parentElement.getBoundingClientRect().width');
  resizeLayouts.push({ before: resize, pointer_width: resized });
  assert.ok(resized >= resize.width + 50, `column drag must change actual layout width: ${JSON.stringify({resize,resized})}`);
  await evaluate('document.querySelector(".column-resize-handle").focus()');
  await press("ArrowLeft", "ArrowLeft", 37);
  await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  await shot("fixture-list-keyboard-resize", 1440, 900);
  const keyboardWidth = await evaluate('document.querySelector(".column-resize-handle").parentElement.getBoundingClientRect().width');
  resizeLayouts.at(-1).keyboard_width = keyboardWidth;
  assert.ok(keyboardWidth < resized, `keyboard resizing must change actual layout width: ${JSON.stringify({resize,resized,keyboardWidth,table:await evaluate('document.querySelector("table").outerHTML')})}`);

  // Hold a real fixture response: the compiled app keeps its list, focus and geometry.
  for (const [width, height] of [[1440, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    await call("Emulation.setEmulatedMedia", { features: [{name: "prefers-reduced-motion", value: "reduce"}, {name: "prefers-color-scheme", value: theme === "auto-light" || theme === "dark" ? "light" : "dark"}] });
    delayNextRead = true;
    await evaluate('(()=>{const input=document.querySelector(".search-input");input.focus();input.value="Loading probe";input.dispatchEvent(new Event("input",{bubbles:true}));input.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));})()');
    // Search facets may legitimately wrap the toolbar before loading feedback appears.
    // Compare after their synchronous render; the loading status itself must add no layout.
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
    await shot(`fixture-loading-baseline-${width}`, width, height);
    const before = await evaluate('document.querySelector("table").getBoundingClientRect().top');
    await wait('document.querySelector(".client-loading")?.hidden === false');
    await shot(`fixture-loading-visible-${width}`, width, height);
    const feedback = await evaluate('(()=>{const node=document.querySelector(".client-loading"),r=node.getBoundingClientRect(),style=getComputedStyle(node);return {top:document.querySelector("table").getBoundingClientRect().top,width:r.width,height:r.height,right:innerWidth-r.right,bottom:innerHeight-r.bottom,pointer:style.pointerEvents,animation:getComputedStyle(node,"::before").animationName,focus:document.activeElement.classList.contains("search-input"),rows:document.querySelectorAll("table tbody tr").length,notice:document.querySelector("[data-client-notice]").textContent,overflow:document.documentElement.scrollWidth>innerWidth,role:node.getAttribute("role"),hit:document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)?.closest(".client-loading")!==null};})()');
    assert.equal(feedback.top, before, "loading must not move the table");
    assert.ok(feedback.width <= 180 && feedback.height <= 40 && feedback.right >= 10 && feedback.right <= 18 && feedback.bottom >= 10 && feedback.bottom <= 18, JSON.stringify(feedback));
    assert.equal(feedback.pointer, "none"); assert.equal(feedback.animation, "none");
    assert.ok(feedback.focus && !feedback.overflow && !feedback.hit, JSON.stringify(feedback));
    assert.equal(feedback.rows, 1); assert.notEqual(feedback.notice, "Loading…"); assert.equal(feedback.role, "status");
    await shot(`fixture-loading-${width}`, width, height);
    await wait('document.querySelector(".client-loading")?.hidden === true');
    await evaluate('document.querySelector(".search-clear-btn").click()');
    await wait('document.querySelectorAll("table tbody tr").length === 1');
  }

  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:0,y:0});
  await evaluate('(()=>{ const input = document.querySelector(".search-input"); input.focus(); input.value = "Fixture"; input.dispatchEvent(new Event("input", { bubbles: true })); })()');
  await wait('document.querySelector(".erp-search-autocomplete .erp-autocomplete-item.active") !== null');
  await shot("fixture-search-autocomplete-desktop", 1440, 900);
  await evaluate('document.querySelector(".search-input").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }))');
  await wait('document.querySelector(".search-facet-chip") !== null && document.querySelector(".empty-state")?.textContent.includes("No matching records")');
  assert.deepEqual(searchDomains.at(-1), [["name", "ilike", "Fixture"]]);
  assert.equal(await evaluate('document.querySelectorAll("table tbody tr").length'), 0);
  const searchedUrl = await evaluate('location.href');
  await call("Page.navigate", { url: searchedUrl });
  await wait('document.querySelector(".search-facet-chip") !== null && document.querySelector(".empty-state")?.textContent.includes("No matching records")');
  assert.deepEqual(searchDomains.at(-1), [["name", "ilike", "Fixture"]]);
  await evaluate('document.querySelector(".search-menu-toggle").click()');
  await wait('document.querySelector(".erp-search-menu-panel")?.style.display !== "none"');
  await shot("fixture-search-menu-desktop", 1440, 900);
  await evaluate('document.querySelector(".erp-search-toggle-form-btn").click()');
  await wait('document.querySelector(".erp-btn-primary") !== null');
  const applyRect = await evaluate('(()=>{const r=document.querySelector(".erp-btn-primary").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()');
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:applyRect.x,y:applyRect.y});
  await shot("fixture-search-apply-hover-desktop",1440,900);
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:0,y:0});
  await evaluate('document.querySelector(".erp-search-toggle-form-btn").click()');
  const searchSections = await evaluate('[...document.querySelector(".erp-search-menu-sections").children].map(e=>{const r=e.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width};})');
  assert.equal(searchSections.length, 3);
  assert.ok(searchSections[0].x < searchSections[1].x && searchSections[1].x < searchSections[2].x, "desktop search menu needs three columns");
  assert.ok(searchSections.every(section=>Math.abs(section.y-searchSections[0].y)<2), "desktop search headings must share a row");
  for (const width of [800, 900]) {
    await shot(`fixture-search-menu-${width}`, width, 900);
    await wait('(()=>{const r=document.querySelector(".erp-search-menu-panel").getBoundingClientRect();return r.left>=0 && r.right<=innerWidth;})()');
    const bounds = await evaluate('(()=>{const r=document.querySelector(".erp-search-menu-panel").getBoundingClientRect();return {left:r.left,right:r.right,width:innerWidth};})()');
    assert.ok(bounds.left >= 0 && bounds.right <= bounds.width, `search menu must fit ${width}px viewport: ${JSON.stringify(bounds)}`);
  }
  await shot("fixture-search-menu-mobile", 375, 812);
  assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'), "mobile search menu must stay within viewport");
  await shot("fixture-search-menu-desktop", 1440, 900);
  await evaluate('document.querySelector(".search-menu-toggle").click()');
  await wait('document.querySelector(".erp-search-menu-panel")?.style.display === "none"');
  await evaluate('document.querySelector(".search-clear-btn")?.click()');
  await wait('document.querySelectorAll("table tbody tr").length === 1');
  assert.deepEqual(searchDomains.at(-1), []);
  assert.equal(await evaluate('[...new URLSearchParams(location.search).keys()].some(key => key.startsWith("search"))'), false);
  assert.equal(await evaluate('new URLSearchParams(location.search).get("menu")'), "10");
  await shot("fixture-list-mobile", 375, 812);
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Settings").click()');
  await wait('document.querySelector(".settings-toggle input") !== null');
  assert.equal(await evaluate(`document.querySelector('[aria-current="page"]')?.textContent`), "Settings");
  await shot("fixture-settings-desktop", 1440, 900); await shot("fixture-settings-tablet", 768, 1024); await shot("fixture-settings-mobile", 375, 812);
  rejectNextAction = true;
  // Use an unopened action: previously visited descriptors can legitimately be cached.
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Restricted").click()');
  await wait('document.querySelector(".client-error-dialog")?.open === true');
  assert.equal(await evaluate('document.querySelector("#client-error-message")?.textContent'), permissionMessage);
  assert.ok(deniedActions.length >= 1, "the denied action was requested instead of using a cached descriptor");
  assert.ok(deniedActions.every(call => call.ids.length === 1 && call.ids[0] === 23), "only the intended Restricted action is denied");
  assert.equal(await evaluate('document.querySelector(".settings-toggle input") !== null'), true, "permission failure preserves the current Settings view");
  await shot("fixture-permission-dialog-mobile",375,812);
  await evaluate('document.querySelector("[data-error-close]").click()');
  await wait('document.querySelector(".client-error-dialog") === null');
  assert.equal(await evaluate(`[...document.querySelectorAll('.settings-toolbar button,.settings-toolbar select')].every(node=>{const box=node.getBoundingClientRect();return document.elementFromPoint(box.x+box.width/2,box.y+box.height/2)===node})`), true);
  assert.equal(await evaluate('document.querySelector(".client-loading").hidden'), true);
  await shot("fixture-settings-recovered-mobile", 375, 812);
  await call("Page.navigate", { url: base + "/erp/action-22" });
  await wait('document.querySelector(".addon-tile") !== null');
  for (const [width,height] of [[1440,900],[768,1024],[375,812]]) {
    await shot(`fixture-addons-${width}`,width,height);
    assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'), `addons fit ${width}px`);
  }
  await evaluate('document.querySelector(".addon-info-btn").click()');
  await wait('document.querySelector(".addon-info-dialog")?.open === true');
  assert.equal(await evaluate('document.activeElement.closest(".addon-info-dialog") !== null'), true);
  await shot("fixture-addon-info-mobile",375,812);
  await evaluate('document.querySelector(".addon-info-dialog button").click()');
  await wait('document.querySelector(".addon-info-dialog") === null');
  assert.equal(await evaluate('document.activeElement.classList.contains("addon-info-btn")'), true);
  }
  assert.deepEqual(exceptions, []);
  const result = { status: dialogProbe ? "DIAGNOSTIC_NOT_ACCEPTANCE" : "PASS", scope: `compiled-asset ${remoteCdp ? "remote CDP" : "Chrome"} rendering with explicit protocol fixtures; not backend/auth/database acceptance`, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), time_utc: new Date().toISOString(), hostname: hostname(), node: process.version, browser: version, theme, office_blue: officeBlue, viewport_only: viewportOnly, office_blue_scope: listOnly ? "list/category/page/count-error/delete-confirmation layout and narrow drawer close; resize/form/settings not run" : "complete fixture", appearance_checks: appearanceChecks, manifest, screenshots, list_layouts: listLayouts, resize_layouts: resizeLayouts, error_layouts: errorLayouts, calls, search_domains: searchDomains, page_reads: pageReads, group_domains: groupDomains, observed_write: writes[0], denied_actions: deniedActions, unhandled_exceptions: exceptions };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n"); console.log(JSON.stringify(result));
} catch (error) {
  try {
    await writeFile(path.join(output, "failure.json"), JSON.stringify({ status: "FAIL", time_utc: new Date().toISOString(), hostname: hostname(), node: process.version, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), message: String(error), unhandled_exceptions: exceptions, screenshots, calls, denied_actions: deniedActions }, null, 2) + "\n");
  } catch (evidenceError) { throw new AggregateError([error, evidenceError], "Browser fixture failed and its failure receipt could not be written"); }
  throw error;
} finally {
  for (const request of pending.values()) clearTimeout(request.timer);
  socket?.close();
  if (chrome?.pid && chrome.exitCode === null && chrome.signalCode === null) {
    const exited = once(chrome, "exit");
    chrome.kill("SIGTERM");
    let timer;
    try { await Promise.race([exited, new Promise(resolve => { timer = setTimeout(resolve, 3000); })]); }
    finally { clearTimeout(timer); }
    if (chrome.exitCode === null && chrome.signalCode === null) { chrome.kill("SIGKILL"); await exited; }
  }
  await new Promise(resolve => server.close(resolve)); await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}

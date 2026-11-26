/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real-server browser evidence for Settings > Technical (review T3).
// Started by web/scripts/browser-human-technical.sh, which serves a fresh database with our own
// `erp server` binary on loopback. This script logs in through the real login form, derives every
// Technical entry from the server (menu tree and external identifiers), cross-checks the entries
// against the menu ids of addons/base/data/technical_views.xml, then opens each entry's list view
// and one record's form view. No fixture server, request interception or fabricated RPC; every
// failure throws and the run is recorded as FAIL.
import { constants } from "node:fs";
import { open, mkdir, mkdtemp, readFile, readdir, writeFile, rm } from "node:fs/promises";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { hostname, platform, tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const known = new Set(["--origin", "--login", "--password-file", "--output", "--technical-xml", "--mail-technical-xml"]);
for (let i = 0; i < args.length; i += 2) if (!known.has(args[i]) || args[i + 1] === undefined) throw new Error(`Unknown or incomplete argument: ${args[i]}`);
const option = name => { const index = args.indexOf(name); return index < 0 ? undefined : args[index + 1]; };
const origin = option("--origin"), login = option("--login"), credentialPath = option("--password-file"), output = option("--output"), xmlPath = option("--technical-xml");
if (!origin || !login || !credentialPath || !output || !xmlPath) throw new Error("Required: --origin http://127.0.0.1:<port> --login <login> --password-file <mode-600 file> --technical-xml <technical_views.xml> --output <directory>");
const originUrl = new URL(origin);
if (originUrl.protocol !== "http:" || originUrl.hostname !== "127.0.0.1" || originUrl.origin !== origin) throw new Error("Origin must be a loopback http://127.0.0.1:<port> server started by browser-human-technical.sh");

let password;
{
  const file = await open(credentialPath, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await file.stat();
    if (!stat.isFile() || (stat.mode & 0o777) !== 0o600 || stat.size === 0 || stat.size > 4096) throw new Error("Password file must be a nonempty mode-600 regular file of at most 4096 bytes");
    password = new TextDecoder("utf-8", { fatal: true }).decode(await file.readFile());
    if (password.endsWith("\n")) password = password.slice(0, -1);
  } finally { await file.close(); }
}

// The menu entries declared by technical_views.xml: every <menuitem>, with its action when it has one.
const xmlText = await readFile(xmlPath, "utf8");
const mailXml = option("--mail-technical-xml") ? await readFile(option("--mail-technical-xml"), "utf8") : "";
const xmlMenus = [];
for (const [module, file] of [["base", xmlText], ["mail", mailXml]]) {
  for (const match of file.matchAll(/<menuitem\b([^>]*)\/?>/g)) {
    const attrs = Object.fromEntries([...match[1].matchAll(/([a-z_]+)="([^"]*)"/g)].map(m => [m[1], m[2]]));
    if (!attrs.id) throw new Error("menuitem without id in technical_views.xml");
    if (attrs.active === "False") continue; // The compatibility alias is intentionally archived.
    xmlMenus.push({ module, id: attrs.id, name: attrs.name, parent: attrs.parent?.replace(/^base\./, ""), action: attrs.action ?? null });
  }
}
if (new Set(xmlMenus.map(m => m.id)).size !== xmlMenus.length) throw new Error("Duplicate menuitem id in technical_views.xml");
const xmlLeaves = xmlMenus.filter(m => m.action !== null);
if (xmlLeaves.length === 0) throw new Error("technical_views.xml declares no menu entry with an action");

const root = fileURLToPath(new URL("../../", import.meta.url));
let source = process.env.ERP_TECHNICAL_REVISION;
try { source ??= (await readFile(path.join(root, "REVISION"), "utf8")).trim(); }
catch (error) { if (error.code !== "ENOENT") throw error; source = undefined; }
if (!source) throw new Error("The browser launcher must provide the source revision.");
const sha256 = data => createHash("sha256").update(data).digest("hex");
const runnerHash = sha256(await readFile(fileURLToPath(import.meta.url)));
const xmlHash = sha256(xmlText + mailXml);

await mkdir(output, { recursive: true, mode: 0o700 });
// Stale files of an earlier run must not sit next to this run's evidence.
for (const name of await readdir(output)) if (name.endsWith(".png") || name === "result.json") await rm(path.join(output, name));

const profile = await mkdtemp(path.join(tmpdir(), "erp-technical-chrome-"));
const chromePath = process.env.CHROME_BIN ?? (platform() === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "google-chrome");
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });

let socket, session, sequence = 0, phase = "startup", browser, serverVersions, failure;
let mailInstalled = false;
const pending = new Map(), steps = [], screenshots = [], httpFailures = [], entries = [], exceptions = [], consoleErrors = [];
const check = (value, label) => { if (!value) throw new Error(label); };
let call, evaluate, shot;
try {
  const endpoint = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Chrome startup deadline")), 15000);
    chrome.once("error", error => { clearTimeout(timer); reject(new Error(`Chrome could not start: ${error.message}`)); });
    chrome.once("exit", code => { clearTimeout(timer); reject(new Error(`Chrome exited before startup (${code})`)); });
    chrome.stderr.on("data", data => { const match = /DevTools listening on (ws:\/\/[^\s]+)/.exec(data.toString()); if (match) { clearTimeout(timer); resolve(match[1]); } });
  });
  socket = new WebSocket(endpoint);
  await Promise.race([once(socket, "open"), new Promise((_, reject) => setTimeout(() => reject(new Error("CDP connection deadline")), 15000))]);
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.id) {
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id); clearTimeout(request.timer);
      if (message.error) {
        const error = new Error(`CDP command failed: ${message.error.message}`);
        error.navigation = /context.*(destroyed|not found)|Cannot find context|Inspected target navigated/.test(message.error.message);
        request.reject(error);
      } else request.resolve(message.result);
    } else if (message.method === "Runtime.exceptionThrown") exceptions.push({ phase, text: message.params.exceptionDetails?.exception?.description ?? message.params.exceptionDetails?.text });
    else if (message.method === "Runtime.consoleAPICalled" && message.params.type === "error") consoleErrors.push({ phase, text: message.params.args.map(arg => arg.value ?? arg.description).join(" ") });
    else if (message.method === "Network.responseReceived" && message.params.response.status >= 400) {
      const url = new URL(message.params.response.url);
      if (url.origin === origin) {
        const status = message.params.response.status;
        const optionalActivity = !mailInstalled && status === 404 && url.pathname === "/erp/json/2/mail.activity/erp_activity_groups";
        const item = { path: url.pathname, status, phase, optionalActivity };
        httpFailures.push(item);
        call("Network.getResponseBody", { requestId: message.params.requestId }).then(res => {
          item.body = res.body;
        }).catch(() => {});
      }
    }
  });
  call = (method, params = {}, scoped = true) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP deadline in ${phase}`)); }, 30000);
    pending.set(id, { resolve, reject, timer });
    socket.send(JSON.stringify({ id, method, params, ...(scoped && session ? { sessionId: session } : {}) }));
  });
  browser = await call("Browser.getVersion", {}, false);
  const target = await call("Target.createTarget", { url: "about:blank" }, false);
  session = (await call("Target.attachToTarget", { targetId: target.targetId, flatten: true }, false)).sessionId;
  await call("Page.enable"); await call("Runtime.enable"); await call("Network.enable");
  await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  evaluate = async expression => {
    const result = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(`Browser evaluation failed in ${phase}: ${result.exceptionDetails.exception?.description ?? result.exceptionDetails.text}`);
    return result.result.value;
  };
  const wait = async (expression, label) => {
    const end = Date.now() + 30000;
    while (Date.now() < end) {
      try { if (await evaluate(expression)) return; }
      catch (error) { if (!error.navigation) throw error; }
      await new Promise(resolve => setTimeout(resolve, 80));
    }
    throw new Error(`Deadline waiting for ${label} in ${phase}`);
  };
  // Clicks the element the selector names with a real mouse press at its centre. Throws when it
  // never appears, has no size, or another element (or a clipping ancestor) takes the hit there,
  // so an entry a user cannot see or reach fails the run.
  const click = async (selector, label) => {
    const expression = `document.querySelector(${JSON.stringify(selector)})`;
    await wait(`Boolean(${expression})`, label);
    // Only the dropdown menu itself is meant to scroll; no generic scrollIntoView, which would also
    // scroll a clipping ancestor and make a hidden entry look reachable.
    const point = await evaluate(`(()=>{const n=${expression},menu=n.closest('.nav-dropdown-menu');if(menu){const r=n.getBoundingClientRect(),m=menu.getBoundingClientRect();if(r.top<m.top)menu.scrollTop-=m.top-r.top;else if(r.bottom>m.bottom)menu.scrollTop+=r.bottom-m.bottom;}const r=n.getBoundingClientRect();if(r.width===0||r.height===0)return {error:'it has no size'};const x=r.left+r.width/2,y=r.top+r.height/2;if(x<0||y<0||x>=innerWidth||y>=innerHeight)return {error:'its centre is outside the viewport'};const top=document.elementFromPoint(x,y);if(!top||!(top===n||n.contains(top)))return {error:'the hit lands on '+(top?top.tagName.toLowerCase()+'.'+top.className:'nothing')};return {x,y};})()`);
    check(!point.error, `${label} is not clickable where it is drawn: ${point.error}`);
    for (const type of ["mouseMoved", "mousePressed", "mouseReleased"]) await call("Input.dispatchMouseEvent", { type, x: point.x, y: point.y, button: type === "mouseMoved" ? "none" : "left", clickCount: type === "mouseMoved" ? 0 : 1 });
  };
  // Same-origin JSON-2 call from the logged-in page, exactly as the client's transport sends it.
  const post = async (route, body) => evaluate(`fetch(${JSON.stringify(route)},{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json','Accept':'application/json'},body:${JSON.stringify(JSON.stringify(body))}}).then(async r=>({status:r.status,body:r.headers.get('content-type')?.includes('application/json')?await r.json():await r.text()}))`);
  const rpc = (model, method, body) => post(`/erp/json/2/${model}/${method}`, body);
  const rpcOk = async (model, method, body) => {
    const reply = await rpc(model, method, body);
    check(reply.status === 200, `${model}/${method} returned ${reply.status}: ${JSON.stringify(reply.body)}`);
    return reply.body;
  };
  // The client is idle at the expected URL: no load in progress and no error notice.
  const settled = async (url, label) => {
    await wait(`location.pathname+location.search===${JSON.stringify(url)}&&document.querySelector('.client-loading')?.textContent!=='Loading…'`, label);
    const notice = await evaluate("(document.querySelector('[data-client-notice]')?.innerText || '').trim()");
    check(notice === "", `${label}: client notice "${notice}"`);
  };
  shot = async name => {
    const image = await call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
    await writeFile(path.join(output, name + ".png"), Buffer.from(image.data, "base64"));
    screenshots.push({ file: name + ".png", width: 1440, height: 900 });
  };
  const done = label => steps.push({ check: label, status: "PASS" });
  const download = async name => {
    for (let attempt = 0; attempt < 100; attempt++) {
      try { return await readFile(path.join(profile, name), "utf8"); }
      catch (error) {
        if (error.code !== "ENOENT") throw error;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
    }
    throw new Error(`Timed out downloading ${name}`);
  };

  phase = "login";
  await call("Page.navigate", { url: origin + "/erp/login" });
  await wait("Boolean(document.querySelector('form[action=\"/erp/login\"] input[name=password]'))", "login form");
  await evaluate(`(()=>{const f=document.querySelector('form[action="/erp/login"]');f.querySelector('input[name=login]').value=${JSON.stringify(login)};f.querySelector('input[name=password]').value=${JSON.stringify(password)};})()`);
  password = "";
  await click("form[action=\"/erp/login\"] button[type=submit]", "Sign in button");
  await wait("location.pathname==='/erp'&&Boolean(document.querySelector('#erp-bootstrap'))&&Boolean(document.querySelector('[data-client-navigation] button'))", "shell after login");
  serverVersions = await evaluate("(()=>{const v=JSON.parse(document.querySelector('#erp-bootstrap').textContent);return {registry_version:v.registry_version,asset_version:v.asset_version,uid:v.identity.uid}})()");
  check(serverVersions.uid > 1, `logged-in user is the superuser (uid ${serverVersions.uid}); evidence needs an ordinary administrator`);
  done("real login form, shell bootstrap of a non-superuser administrator");

  phase = "operator fixtures";
  const modules = ["sample_library", "technical_lifecycle", ...(mailXml ? ["mail"] : [])];
  const installed = await post("/erp/module/install", { modules });
  check(installed.status === 200, `operator fixtures did not install: ${JSON.stringify(installed.body)}`);
  mailInstalled = Boolean(mailXml);
  await call("Page.reload");
  await wait("location.pathname==='/erp'&&Boolean(document.querySelector('.app-tile'))", "shell after addon installation");
  const parent = (await rpcOk("res.partner", "search_read", {fields: ["name"], limit: 1}))[0];
  check(parent?.id > 0, "attachment fixture needs an accessible contact");
  await rpcOk("ir.config_parameter", "set_int", {
    key: "base.partition_upkeep_interval_secs", value: 900
  });
  const scope = {kind: "global"};
  const key = "base.report_barcode_dpi";
  const setting = await rpcOk("ir.config.setting", "erp_read_values", {scope, keys: [key]});
  await rpcOk("ir.config.setting", "erp_write_values", {
    scope, expected_revision: setting.revision, changes: [{key, value: 203}]
  });
  let attachment;
  if (mailXml) {
    const file = await rpcOk("res.partner", "thread_upload", {
      ids: [parent.id], name: "maintenance.txt", mimetype: "text/plain", data: "bWFpbnRlbmFuY2UgY2hlY2tsaXN0"
    });
    attachment = file.attachment_id;
    await rpcOk("res.partner", "thread_post", {
      ids: [parent.id], post: {subject: "Technical administration example", body: "Mail detail proof", attachment_ids: [attachment]}
    });
    await call("Browser.setDownloadBehavior", {behavior: "allow", downloadPath: profile}, false);
  } else {
    await rpcOk("ir.attachment", "create", { vals_list: [{
      name: "Maintenance checklist", type: "url", url: "https://example.invalid/checklist",
      res_model: "res.partner", res_id: parent.id
    }] });
  }
  done(`installed ${mailXml ? "the mail and report fixtures" : "the report fixture"} and created representative attachment and settings records`);

  phase = "derive-entries";
  // Server side: the administrator's menu tree and the external identifiers of base's Technical menus.
  const tree = await rpcOk("ir.ui.menu", "erp_load_web_menus", {});
  const nodes = new Map(tree.nodes.map(node => [node.id, node]));
  const xmlIds = await rpcOk("ir.model.data", "search_read", { domain: [["module", "in", ["base", "mail"]], ["model", "=", "ir.ui.menu"], ["name", "like", "technical"]], fields: ["name", "res_id"] });
  const menuIdOf = new Map(xmlIds.map(row => [row.name, row.res_id]));
  const serverNames = [...menuIdOf.keys()].sort(), declared = xmlMenus.map(m => m.id).sort();
  check(JSON.stringify(serverNames) === JSON.stringify(declared), `server menu xml ids differ from technical_views.xml: missing ${JSON.stringify(declared.filter(n => !menuIdOf.has(n)))}, extra ${JSON.stringify(serverNames.filter(n => !declared.includes(n)))}`);
  const technicalId = menuIdOf.get("technical_menu");
  check(nodes.has(technicalId), "Technical menu is not in the administrator's menu tree");
  let settingsRoot = nodes.get(technicalId);
  while (settingsRoot.parent_id !== null) { settingsRoot = nodes.get(settingsRoot.parent_id); check(settingsRoot, "Technical menu's ancestor missing from the tree"); }
  // Every leaf declared in the XML: visible in the tree with its action, or hidden for a recorded reason.
  const visible = [], hidden = [];
  for (const leaf of xmlLeaves) {
    const id = menuIdOf.get(leaf.id), node = nodes.get(id);
    if (node) {
      check(node.action_id !== null, `${leaf.id}: visible without an action`);
      check(node.parent_id === menuIdOf.get(leaf.parent), `${leaf.id}: parent in the tree is not ${leaf.parent}`);
      visible.push({ ...leaf, menuId: id, actionId: node.action_id, label: node.label });
    } else hidden.push({ ...leaf, menuId: id });
  }
  done(`server menus match technical_views.xml: ${xmlMenus.length} menu ids, ${xmlLeaves.length} entries, ${visible.length} visible to the administrator`);

  check(hidden.length === 0, `technical menus hidden from the administrator: ${JSON.stringify(hidden)}`);

  phase = "settings";
  await click(`.app-tile[data-menu-id="${settingsRoot.id}"]`, "Settings app tile");
  await wait("location.pathname.startsWith('/erp/action-')&&document.querySelector('.client-loading')?.textContent!=='Loading…'", "Settings app loaded");
  const settingsNotice = await evaluate("(document.querySelector('[data-client-notice]')?.innerText || '').trim()");
  check(settingsNotice === "", `Settings app: client notice "${settingsNotice}"`);
  await wait(`Boolean(document.querySelector('.nav-dropdown [data-menu-id="${visible[0].menuId}"]'))`, "Technical dropdown");
  const dropdown = `[...document.querySelectorAll('.nav-dropdown')].find(d=>d.querySelector('[data-menu-id="${visible[0].menuId}"]'))`;
  const domItems = await evaluate(`[...${dropdown}.querySelectorAll('.dropdown-item')].map(n=>Number(n.dataset.menuId))`);
  check(visible.every(entry => domItems.includes(entry.menuId)), `Technical dropdown is missing visible entries: ${JSON.stringify(domItems)}`);
  const toggleLabel = await evaluate(`${dropdown}.querySelector('.nav-dropdown-toggle').textContent`);
  check(toggleLabel.trim().replace(/\s*▾$/, "") === nodes.get(technicalId).label, `Technical toggle label is "${toggleLabel}"`);
  const headers = await evaluate(`[...${dropdown}.querySelectorAll('.dropdown-header')].map(n=>Number(n.dataset.menuId))`);
  const expectedHeaders = [...new Set(visible.map(v => menuIdOf.get(v.parent)))];
  check(JSON.stringify([...headers].sort((a, b) => a - b)) === JSON.stringify(expectedHeaders.sort((a, b) => a - b)), `category headers ${JSON.stringify(headers)} differ from the categories of visible entries`);
  const toggleSelector = `.nav-dropdown:has([data-menu-id="${visible[0].menuId}"]) > .nav-dropdown-toggle`;
  await click(toggleSelector, "Technical dropdown toggle");
  await wait(`${dropdown}.querySelector('.nav-dropdown-menu').classList.contains('open')`, "Technical dropdown open");
  await shot("00-technical-dropdown");
  done(`Technical dropdown includes the ${visible.length} checked entries under ${headers.length} category headers`);

  // Phone Technical navigation capture under the new theme.
  phase = "phone navigation";
  if (await evaluate(`${dropdown}.querySelector('.nav-dropdown-menu').classList.contains('open')`)) {
    await click(toggleSelector, "Technical dropdown toggle (close)");
    await wait(`!${dropdown}.querySelector('.nav-dropdown-menu').classList.contains('open')`, "Technical dropdown closed");
  }
  await call("Emulation.setDeviceMetricsOverride", { width: 375, height: 812, deviceScaleFactor: 2, mobile: true });
  await wait("Boolean(document.querySelector('.burger-btn'))", "burger button");
  await click(".burger-btn", "burger button");
  await wait("Boolean(document.querySelector('.mobile-drawer:not(.hidden) .mobile-drawer-sections'))", "mobile drawer open");
  const phoneHasTechnical = await evaluate("Boolean(document.querySelector('.mobile-drawer-sections')?.textContent?.includes('Technical'))");
  check(phoneHasTechnical, "mobile drawer sections include Technical");
  await shot("00-technical-mobile-drawer");
  await click(".drawer-close-btn", "close mobile drawer");
  await wait("Boolean(document.querySelector('.mobile-drawer.hidden'))", "mobile drawer closed");
  await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  done("phone Technical navigation captured in mobile drawer");

  const openTechnicalMenu = async (menuId, label) => {
    if (!await evaluate(`${dropdown}.querySelector('.nav-dropdown-menu').classList.contains('open')`)) {
      await click(toggleSelector, "Technical dropdown toggle");
    }
    await click(`.nav-dropdown .dropdown-item[data-menu-id="${menuId}"]`, label);
  };
  let index = 0;
  for (const entry of visible) {
    index++;
    const prefix = String(index).padStart(2, "0") + "-" + entry.id;
    phase = `entry ${entry.id}`;
    const record = { menu_xmlid: entry.id, label: entry.label, menu_id: entry.menuId, action_xmlid: entry.action, action_id: entry.actionId };
    entries.push(record);
    // Open it from the navigation, as a user does.
    await openTechnicalMenu(entry.menuId, `${entry.id} menu item`);
    if (entry.id === "technical_sessions_menu") {
      await click(".technical-session-open", "active session row");
      await wait("Boolean(document.querySelector('.technical-session-detail'))", "browser session and device details");
      const detail = await evaluate("document.querySelector('.technical-session-detail').innerText");
      check(detail.includes(login) && detail.includes("Trusted device") && detail.includes("End Session"), "session details or control missing");
      await shot(prefix + "-detail");
      record.form = { state: "opened", labels: ["Trusted device", "Browser", "End Session"] };
      continue;
    }
    if (entry.id === "technical_jobs_settings_menu") {
      await wait("Boolean(document.querySelector('.settings-form'))", "Background Job Settings service");
      record.form = { state: "opened", labels: ["Background Job Settings"] };
      await shot(prefix + "-settings");
      continue;
    }
    const action = await rpcOk("ir.actions.act_window", "erp_load_web_action", { ids: [entry.actionId] });
    record.model = action.model;
    const listUrl = `/erp/action-${entry.actionId}?menu=${entry.menuId}`;
    await settled(listUrl, `${entry.id} list`);
    await wait("Boolean(document.querySelector('[data-client-content] table[aria-label=Records]'))", `${entry.id} list table`);
    const list = await evaluate("(()=>{const t=document.querySelector('[data-client-content] table[aria-label=Records]');return {columns:[...t.querySelectorAll('thead th')].map(n=>n.textContent.trim()),rows:t.querySelectorAll('tbody tr').length,pager:document.querySelector('[data-client-content] .pager-count, .pager-count')?.textContent??null,title:document.querySelector('[data-control-breadcrumbs]')?.textContent.trim()??null}})()");
    const count = await rpcOk(action.model, "search_count", { domain: [] });
    record.list = { url: listUrl, columns: list.columns, rows_shown: list.rows, pager: list.pager, breadcrumb: list.title, search_count: count };
    check(list.columns.length > 0, `${entry.id}: list has no columns`);
    await shot(prefix + "-list");
    if (list.rows === 0) {
      check(count === 0, `${entry.id}: list shows no row but search_count is ${count}`);
      record.form = { state: "not opened", reason: "the model has no record in this database (search_count 0)" };
      continue;
    }
    check(count > 0, `${entry.id}: list shows rows but search_count is ${count}`);
    await click("[data-client-content] table[aria-label=Records] tbody tr button.link", `${entry.id} first record link`);
    await wait(`new URLSearchParams(location.search).get('view_type')==='form'&&location.pathname===${JSON.stringify(`/erp/action-${entry.actionId}`)}`, `${entry.id} form route`);
    const id = await evaluate("new URLSearchParams(location.search).get('id')");
    check(/^[1-9][0-9]*$/.test(id ?? ""), `${entry.id}: form route has no record id`);
    await settled(`/erp/action-${entry.actionId}?menu=${entry.menuId}&id=${id}&view_type=form`, `${entry.id} form`);
    await wait("Boolean(document.querySelector('[data-client-content] form.record-form'))", `${entry.id} record form`);
    const form = await evaluate("(()=>{const f=document.querySelector('[data-client-content] form.record-form');return {labels:[...f.querySelectorAll('label')].map(n=>n.textContent.trim()).filter(Boolean),inputs:f.querySelectorAll('input,select,textarea').length,breadcrumb:document.querySelector('[data-control-breadcrumbs]')?.textContent.trim()??null}})()");
    check(form.labels.length > 0 || form.inputs > 0, `${entry.id}: form shows no field`);
    record.form = { state: "opened", record_id: Number(id), labels: form.labels, inputs: form.inputs, breadcrumb: form.breadcrumb };
    await shot(prefix + "-form");
    if (entry.id === "technical_attachments_menu" && attachment !== undefined) {
      check(Number(id) === attachment, "the technical attachment list opened the seeded binary record");
      await click(".technical-attachment-download", "Download attachment");
      check(await download("maintenance.txt") === "maintenance checklist", "downloaded attachment bytes differ");
      done("opened a listed attachment and downloaded its original bytes through the content route");
    }
  }
  const opened = entries.filter(e => e.form?.state === "opened").length, empty = entries.filter(e => e.form?.state === "not opened").length;
  const lists = entries.filter(e => e.list !== undefined).length;
  const forms = entries.filter(e => e.form?.record_id !== undefined).length;
  done(`${entries.length} Technical actions opened; ${lists} record lists, ${forms} record forms, ${opened - forms} service details; ${empty} lists without records`);

  phase = "cancel uninstall";
  const menuId = (await rpcOk("ir.model.data", "search_read", {
    domain: [["module", "=", "web"], ["model", "=", "ir.ui.menu"], ["name", "=", "addons_menu"]], fields: ["res_id"], limit: 1,
  }))[0]?.res_id;
  check(menuId > 0, "Addons menu missing");
  await openTechnicalMenu(menuId, "Addons");
  await wait("Boolean(document.querySelector('.addons-search'))", "addon catalog search");
  await evaluate("(()=>{const search=document.querySelector('.addons-search');search.value='technical_lifecycle';search.dispatchEvent(new Event('input',{bubbles:true}));})()");
  const testAddon = '.addon-card[data-addon="technical_lifecycle"] .addon-uninstall';
  await click(testAddon, "Uninstall lifecycle fixture");
  await wait("Boolean(document.querySelector('.addon-uninstall-dialog[open]'))", "live uninstall preview dialog");
  const impact = await evaluate("document.querySelector('.addon-uninstall-dialog').innerText");
  check(impact.includes("technical_lifecycle") && impact.includes("Package and host effects") && impact.includes("Web assets"), "uninstall preview effects missing");
  await shot("uninstall-preview");
  await click(".addon-uninstall-cancel", "Cancel uninstall");
  await wait("!document.querySelector('.addon-uninstall-dialog')", "cancelled uninstall");
  const addon = await rpcOk("ir.module.module", "erp_addon_detail", { name: "technical_lifecycle" });
  check(addon.state === "installed", "cancellation changed the addon state");
  done("cancelled a real engine preview without uninstalling the addon");

  phase = "translation catalog";
  const translationMenu = (await rpcOk("ir.model.data", "search_read", {
    domain: [["module", "=", "base"], ["model", "=", "ir.ui.menu"], ["name", "=", "translation_catalog_menu"]], fields: ["res_id"], limit: 1,
  }))[0]?.res_id;
  check(translationMenu > 0, "Translation Catalog menu missing");
  const language = (await rpcOk("res.lang", "search_read", {
    domain: [["code", "=", "fr_FR"]], fields: ["code"], context: {active_test: false}, limit: 1,
  }))[0];
  check(language?.id > 0, "French language is missing");
  await rpcOk("res.lang", "write", {ids: [language.id], vals: { active: true }});
  const translationToggle = `.nav-dropdown:has([data-menu-id="${translationMenu}"]) > .nav-dropdown-toggle`;
  await click(translationToggle, "Reference Data dropdown");
  await click(`.dropdown-item[data-menu-id="${translationMenu}"]`, "Translation Catalog");
  await wait("Boolean(document.querySelector('.translation-catalog'))", "translation catalog form");
  await evaluate(`(()=>{const form=document.querySelector('.translation-catalog');
    const [module, model] = form.querySelectorAll('input');
    module.value='base';model.value='res.country';form.querySelector('select').value='fr_FR';
    for (const control of [module, model, form.querySelector('select')]) control.dispatchEvent(new Event('change',{bubbles:true}));
  })()`);
  const glossary = path.join(profile, "translation.po");
  await writeFile(glossary, '#: model:res.country,name:base.ad\nmsgid "Andorra"\nmsgstr "Andorre"\n');
  const { objectId } = (await call("Runtime.evaluate", {expression: "document.querySelector('.translation-catalog input[type=file]')"})).result;
  await call("DOM.setFileInputFiles", {files: [glossary], objectId});
  await click(".translation-catalog .primary", "Import PO");
  await click(".message-dialog button[data-default-action]", "Confirm PO import");
  await wait("document.querySelector('[data-client-notice]')?.innerText.includes('Imported 1 translation terms')", "translation import confirmation");
  await call("Browser.setDownloadBehavior", {behavior: "allow", downloadPath: profile}, false);
  await click(".translation-export", "Export PO");
  const exported = await download("base-res.country-fr_FR.po");
  check(exported.includes('msgstr "Andorre"'), "downloaded catalog is missing the imported translation");
  await shot("translation-catalog");
  done("imported a gettext catalog from the browser and verified its stored translation in the export");

  phase = "errors";
  check(exceptions.length === 0, `uncaught page exceptions: ${JSON.stringify(exceptions)}`);
  const unexpected = httpFailures.filter(item => item.path !== "/favicon.ico" && !item.optionalActivity);
  check(unexpected.length === 0, `unexpected HTTP errors: ${JSON.stringify(unexpected)}`);
  check(consoleErrors.length === 0, `console errors: ${JSON.stringify(consoleErrors)}`);
  done("no page exception, console error or unexpected HTTP error");
  phase = "complete";
} catch (error) {
  failure = { phase, message: error instanceof Error ? error.message : String(error) };
  if (shot && session) { try { await shot("failure"); } catch { /* the original failure is what the result records */ } }
} finally {
  const result = {
    functional: phase === "complete" ? "PASS" : "FAIL",
    failure: failure ?? null,
    tool: "web/scripts/browser-human-technical.mjs",
    runner_sha256: runnerHash,
    source_revision: source,
    source_dirty: source.includes("-dirty"),
    technical_xml_sha256: xmlHash,
    time_utc: new Date().toISOString(),
    host: hostname(),
    node: process.version,
    browser: browser ? { product: browser.product, user_agent: browser.userAgent, protocol: browser.protocolVersion } : null,
    server: { origin, versions: serverVersions ?? null },
    login,
    steps,
    entries,
    screenshots,
    exceptions,
    console_errors: consoleErrors,
    http_failures: httpFailures,
    scope: "Desktop 1440x900 and phone 375x812 Technical navigation. Other mobile layouts, axe/contrast and keyboard navigation not covered by this run.",
    performance: "DEFERRED_PERFORMANCE; no timing recorded",
  };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify({ functional: result.functional, failure: result.failure, steps: steps.length, entries: entries.length, screenshots: screenshots.length }));
  for (const request of pending.values()) clearTimeout(request.timer);
  socket?.close();
  if (chrome.pid && chrome.exitCode === null) {
    chrome.kill("SIGTERM");
    await Promise.race([once(chrome, "exit"), new Promise(resolve => setTimeout(resolve, 3000))]);
    if (chrome.exitCode === null) { chrome.kill("SIGKILL"); await Promise.race([once(chrome, "exit"), new Promise(resolve => setTimeout(resolve, 3000))]); }
  }
  // Chrome's helper processes can still be flushing the profile right after the browser exits.
  await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
  if (phase !== "complete") process.exitCode = 1;
}

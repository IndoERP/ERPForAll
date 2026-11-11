/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real deployed browser acceptance. No fixture server, request interception, or fabricated RPC.
// Run only against the owner's dedicated acceptance database with --read-only.
import { constants } from "node:fs";
import { open, mkdir, mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { hostname, tmpdir, platform } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const option = name => { const index = args.indexOf(name); return index < 0 ? undefined : args[index + 1]; };
const stageDelayMs = option("--stage-delay-ms") === undefined ? 0 : Number(option("--stage-delay-ms"));
if (!Number.isInteger(stageDelayMs) || stageDelayMs < 0 || stageDelayMs > 1000) throw new Error("Stage delay must be an integer from 0 to 1000 milliseconds.");
const credentialPath = option("--password-file"), output = option("--output");
if (!credentialPath || !output || !option("--origin") || !option("--login") || !args.includes("--read-only")) throw new Error("Required: --origin <https-origin> --login <acceptance-login> --password-file <protected-file> --output <private-directory> --read-only");
let credentials;
try {
  const file = await open(credentialPath, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await file.stat();
    if (!stat.isFile() || (stat.mode & 0o777) !== 0o600 || stat.size > 4096) throw new Error();
    const buffer = Buffer.alloc(4097);
    const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
    if (bytesRead > 4096) throw new Error();
    let password = new TextDecoder("utf-8", { fatal: true }).decode(buffer.subarray(0, bytesRead));
    if (password.endsWith("\n")) password = password.slice(0, -1);
    credentials = { origin: option("--origin"), login: option("--login"), password };
  } finally { await file.close(); }
  if (!["origin", "login", "password"].every(key => typeof credentials[key] === "string" && credentials[key].length > 0)) throw new Error();
  const url = new URL(credentials.origin);
  if (url.protocol !== "https:" || url.username || url.password || url.origin !== credentials.origin) throw new Error();
} catch { throw new Error("Password file must be bounded mode-600 regular UTF-8; origin must be HTTPS and login/password nonempty."); }
await mkdir(output, { recursive: true, mode: 0o700 });
const profile = await mkdtemp(path.join(tmpdir(), "erp-live-chrome-"));
const root = fileURLToPath(new URL("../../", import.meta.url));
let source, dirty;
try { source = (await readFile(path.join(root, "REVISION"), "utf8")).trim(); dirty = source.includes("-dirty-"); }
catch { source = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim(); dirty = execFileSync("git", ["status", "--porcelain", "--untracked-files=no"], { cwd: root, encoding: "utf8" }).trim().length > 0; }
const runnerHash = createHash("sha256").update(await readFile(fileURLToPath(import.meta.url))).digest("hex");
const chromePath = process.env.CHROME_BIN ?? (platform() === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "chromium");
const chrome = spawn(chromePath, ["--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
let socket, session, sequence = 0, phase = "startup", browser, recordId, settingsBaseline;
const pending = new Map(), steps = [], screenshots = [], requests = [], httpFailures = [];
const waterfall = [], inflight = new Map(); let failure;
let exceptionCount = 0, consoleErrorCount = 0, serverVersions, failedCheck;
const check = (value, label) => { if (!value) { failedCheck = label; throw new Error(label); } };
const button = (label, scope = "document") => `[...${scope}.querySelectorAll('button')].find(node=>node.textContent.trim()===${JSON.stringify(label)}&&!node.disabled)`;
let call, evaluate, wait, shot;
try {
  const endpoint = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Chrome startup deadline")), 15000);
    chrome.once("error", () => { clearTimeout(timer); reject(new Error("Chrome could not start")); });
    chrome.once("exit", () => { clearTimeout(timer); reject(new Error("Chrome exited before startup")); });
    chrome.stderr.on("data", data => { const match = /DevTools listening on (ws:\/\/[^\s]+)/.exec(data.toString()); if (match) { clearTimeout(timer); resolve(match[1]); } });
  });
  socket = new WebSocket(endpoint);
  await Promise.race([once(socket, "open"), new Promise((_, reject) => setTimeout(() => reject(new Error("CDP connection deadline")), 15000))]);
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.method === "Network.requestWillBeSent") {
      const request = message.params.request, url = new URL(request.url);
      if (url.origin === credentials.origin) { const item = { path: url.pathname, method: request.method, phase, start: message.params.timestamp }; waterfall.push(item); inflight.set(message.params.requestId, item); }
    } else if (message.method === "Network.responseReceived") {
      const item = inflight.get(message.params.requestId), response = message.params.response;
      if (item) { const headers = Object.fromEntries(Object.entries(response.headers).map(([key,value])=>[key.toLowerCase(),value])); Object.assign(item,{status:response.status,protocol:response.protocol,fromDiskCache:response.fromDiskCache,fromServiceWorker:response.fromServiceWorker,timing:response.timing,cache_control:headers['cache-control'],content_encoding:headers['content-encoding'],content_length:headers['content-length'],content_type:headers['content-type']}); }
    } else if (message.method === "Network.loadingFinished") {
      const item = inflight.get(message.params.requestId); if(item) { item.duration_ms=(message.params.timestamp-item.start)*1000; item.encoded_bytes=message.params.encodedDataLength; }
    }
    if (message.id) {
      const request = pending.get(message.id); if (!request) return;
      pending.delete(message.id); clearTimeout(request.timer);
      if (message.error) {
        const error = new Error("CDP command failed");
        error.navigation = /context.*(destroyed|not found)|Cannot find context|Inspected target navigated/.test(message.error.message);
        request.reject(error);
      } else request.resolve(message.result);
    } else if (message.method === "Runtime.exceptionThrown") exceptionCount++;
    else if (message.method === "Runtime.consoleAPICalled" && message.params.type === "error") consoleErrorCount++;
    else if (message.method === "Network.requestWillBeSent") {
      const url = new URL(message.params.request.url);
      if (url.origin === credentials.origin) requests.push({ path: url.pathname, method: message.params.request.method, phase });
    } else if (message.method === "Network.responseReceived" && message.params.response.status >= 400) {
      const url = new URL(message.params.response.url);
      if (url.origin === credentials.origin) httpFailures.push({ path: url.pathname, status: message.params.response.status, phase });
    }
  });
  call = (method, params = {}, scoped = true) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP deadline in ${phase}`)); }, 20000);
    pending.set(id, { resolve, reject, timer }); socket.send(JSON.stringify({ id, method, params, ...(scoped && session ? { sessionId: session } : {}) }));
  });
  browser = await call("Browser.getVersion", {}, false);
  const target = await call("Target.createTarget", { url: "about:blank" }, false);
  session = (await call("Target.attachToTarget", { targetId: target.targetId, flatten: true }, false)).sessionId;
  await call("Page.enable"); await call("Runtime.enable"); await call("Network.enable");
  evaluate = async expression => {
    const result = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(`Browser evaluation failed in ${phase}`);
    return result.result.value;
  };
  wait = async expression => {
    const end = Date.now() + 30000;
    while (Date.now() < end) {
      try { if (await evaluate(expression)) return; }
      catch (error) { if (!error.navigation) throw error; }
      await new Promise(resolve => setTimeout(resolve, 80));
    }
    throw new Error(`UI readiness deadline in ${phase}`);
  };
  const navigate = async url => {
    await evaluate("window.__erpAcceptanceNavigation=true");
    await call(url ? "Page.navigate" : "Page.reload", url ? { url } : {});
    await wait("window.__erpAcceptanceNavigation!==true&&document.readyState!=='loading'");
  };
  const click = async (label, scope) => { const expression = button(label, scope); await wait(`Boolean(${expression})`); await evaluate(`${expression}.click()`); };
  const nav = async label => {
    await wait("document.querySelector('[data-client-notice]')?.textContent!=='Loading…'");
    await click(label, "document.querySelector('[data-client-navigation]')");
  };
  const saveNotice = async expected => {
    // A prior successful toast must not satisfy a later save before its request commits.
    await evaluate(`(()=>{window.__acceptanceSaved=false;const node=document.querySelector('[data-client-notice]');const observer=new MutationObserver(()=>{if(node.textContent===${JSON.stringify(expected)}){window.__acceptanceSaved=true;observer.disconnect();}});observer.observe(node,{childList:true,characterData:true,subtree:true});})()`);
    await click("Save", "document.querySelector('[data-client-content]')");
    await wait("window.__acceptanceSaved===true");
  };
  const field = ".acceptance-note-title input";
  const fill = async (selector, value) => {
    await wait(`Boolean(document.querySelector(${JSON.stringify(selector)})&&!document.querySelector(${JSON.stringify(selector)}).disabled)`);
    await evaluate(`(()=>{const input=document.querySelector(${JSON.stringify(selector)});input.focus();input.value=${JSON.stringify(value)};input.dispatchEvent(new Event('change',{bubbles:true}));})()`);
  };
  const done = label => steps.push({ check: label, status: "PASS" });
  shot = async (name, width = 1440, height = 900) => {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 700 });
    // Hide credential fields and identity/related-user labels without serializing their values.
    await evaluate(`window.__shotMasks=[...document.querySelectorAll('input[name=login],input[name=password],.identity,.relation-chip,select[aria-label="Current company"]')].map(node=>{const prior=node.style.visibility;node.style.visibility='hidden';return [node,prior]});undefined`);
    try {
      const image = await call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
      await writeFile(path.join(output, name + ".png"), Buffer.from(image.data, "base64"), { mode: 0o600 });
      screenshots.push({ file: name + ".png", width, height });
    } finally { await evaluate("for(const [node,prior] of window.__shotMasks)node.style.visibility=prior;delete window.__shotMasks"); }
  };
  const responsive = async name => { await shot(name + "-desktop"); await shot(name + "-tablet", 768, 1024); await shot(name + "-mobile", 375, 812); await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false }); };
  await call("Performance.enable");
  const metrics = async () => Object.fromEntries((await call("Performance.getMetrics")).metrics.map(item=>[item.name,item.value]));
  const stage = async (name, work) => {
    // Human-paced admission check; excluded from both elapsed and CPU counters.
    if (stageDelayMs) await new Promise(resolve => setTimeout(resolve, stageDelayMs));
    phase=name; const before=await metrics(), start=performance.now(); await work();
    const after=await metrics(); steps.push({phase:name,elapsed_ms:performance.now()-start,metrics_delta:Object.fromEntries(['TaskDuration','ScriptDuration','LayoutDuration','RecalcStyleDuration','LayoutCount','RecalcStyleCount'].map(key=>[key,after[key]-before[key]])),documents:after.Documents,nodes:after.Nodes,js_heap_used:after.JSHeapUsedSize});
  };
  const navigation = async () => evaluate("performance.getEntriesByType('navigation').map(n=>({type:n.type,duration:n.duration,ttfb:n.responseStart-n.requestStart,request_start:n.requestStart,response_start:n.responseStart,response_end:n.responseEnd,dom_content_loaded:n.domContentLoadedEventEnd,load:n.loadEventEnd,transfer_bytes:n.transferSize,encoded_body_bytes:n.encodedBodySize,decoded_body_bytes:n.decodedBodySize}))");
  await stage('cold-login-page',async()=>{await navigate(credentials.origin+'/erp/login');await wait("Boolean(document.querySelector('input[name=password]'))");});
  const loginNavigation=await navigation();
  await fill('input[name=login]',credentials.login);await fill('input[name=password]',credentials.password);
  await stage('login-submit-shell',async()=>{await click('Sign in');await wait("Boolean(document.querySelector('[data-client-navigation] button'))&&document.querySelector('[data-client-notice]')?.textContent===''");});
  credentials.password=''; const shellNavigation=await navigation();
  serverVersions=await evaluate("(()=>{const b=JSON.parse(document.querySelector('#erp-bootstrap').textContent);return {registry_version:b.registry_version,asset_version:b.asset_version}})()");
  await evaluate("window.__longTasks=[];try{new PerformanceObserver(list=>window.__longTasks.push(...list.getEntries().map(e=>({start:e.startTime,duration:e.duration})))).observe({type:'longtask',buffered:true})}catch{};undefined");
  for(let round=1;round<=3;round++) {
    await stage('settings-'+round,async()=>{await nav('Settings');await wait("Boolean(document.querySelector('input[name=\"acceptance_notes.enabled\"]'))&&document.querySelector('[data-client-notice]')?.textContent===''");});
    if(round===1) await responsive('baseline-settings');
    await stage('notes-list-'+round,async()=>{await nav('Notes');await wait("Boolean(document.querySelector('table[aria-label=Records]'))&&document.querySelector('[data-client-notice]')?.textContent===''");});
    if(round===1) await responsive('baseline-list');
    await stage('notes-new-form-'+round,async()=>{await click('New',"document.querySelector('[data-client-content]')");await wait("Boolean(document.querySelector('.acceptance-note-title input'))&&document.querySelector('[data-client-notice]')?.textContent===''");});
    if(round===1) await responsive('baseline-form');
  }
  // No field editing, Save, install, Delete or database reset is performed.
  for(const label of ['Users','Companies','Languages','Groups','Access Rules','Addons']) {
    await stage('admin-'+label.toLowerCase().replaceAll(' ','-'),async()=>{await nav(label);await wait("document.querySelector('[data-client-notice]')?.textContent===''");});
  }
  const longTasks=await evaluate('window.__longTasks');
  await stage('warm-reload-shell',async()=>{await navigate();await wait("Boolean(document.querySelector('[data-client-navigation] button'))&&document.querySelector('[data-client-notice]')?.textContent===''");});
  const warmNavigation=await navigation();
  phase='complete';
  globalThis.pageTimings={loginNavigation,shellNavigation,warmNavigation,longTasks};
} catch(error) { failure=failedCheck??('Read-only baseline stopped in '+phase); if(shot&&session){try{await shot('baseline-failure')}catch{}} }
finally {
 const result={status:phase==='complete'?'COMPLETE':'FAIL',failed_phase:phase==='complete'?null:phase,failure,time_utc:new Date().toISOString(),host:hostname(),node:process.version,source_stamp:source,runner_sha256:runnerHash,browser,server_versions:serverVersions,stage_delay_ms:stageDelayMs,pacing:stageDelayMs?'Paced admission check; delay outside measured stages':'Unpaced diagnostic',stages:steps,waterfall,page_timings:globalThis.pageTimings,screenshots,exception_count:exceptionCount,console_error_count:consoleErrorCount,http_failures:httpFailures,read_only:true,notes:'Single-user diagnostic sample on shared live TEST host; no comparative performance claim or milestone gate.'};
 await writeFile(path.join(output,'baseline.json'),JSON.stringify(result,null,2)+'\n',{mode:0o600});
 console.log(JSON.stringify({status:result.status,failed_phase:result.failed_phase,stages:steps.length,requests:waterfall.length}));
 for(const item of pending.values())clearTimeout(item.timer);socket?.close();
 if(chrome.pid&&chrome.exitCode===null){chrome.kill('SIGTERM');await Promise.race([once(chrome,'exit'),new Promise(resolve=>setTimeout(resolve,3000))]);if(chrome.exitCode===null)chrome.kill('SIGKILL');}
 await rm(profile,{recursive:true,force:true});if(phase!=='complete')process.exitCode=1;
}

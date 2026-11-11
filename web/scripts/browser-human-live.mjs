/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real deployed browser acceptance. No fixture server, request interception, or fabricated RPC.
// Run only against the owner's dedicated acceptance database with --allow-write.
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
const credentialPath = option("--password-file"), output = option("--output");
if (!credentialPath || !output || !option("--origin") || !option("--login") || !args.includes("--allow-write")) throw new Error("Required: --origin <https-origin> --login <acceptance-login> --password-file <protected-file> --output <private-directory> --allow-write");
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
    // Notices render as toasts inside the notice region (message text plus a close control), so match the
    // toast message itself; a re-added toast with the same text is a new node, which the observer still sees.
    await evaluate(`(()=>{window.__acceptanceSaved=false;const node=document.querySelector('[data-client-notice]');const fresh=added=>added.nodeType===1&&[...(added.matches('.client-notification')?[added]:added.querySelectorAll('.client-notification'))].some(toast=>toast.querySelector('.client-notification-message')?.textContent===${JSON.stringify(expected)});const observer=new MutationObserver(records=>{if(records.some(record=>[...record.addedNodes].some(fresh))){window.__acceptanceSaved=true;observer.disconnect();}});observer.observe(node,{childList:true,subtree:true});})()`);
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
  phase = "login";
  await navigate(credentials.origin + "/erp/login");
  await wait("Boolean(document.querySelector('form[action=\"/erp/login\"] input[name=password]'))");
  await responsive("login");
  await fill("input[name=login]", credentials.login); await fill("input[name=password]", credentials.password);
  await click("Sign in"); await wait("Boolean(document.querySelector('[data-client-navigation] button'))");
  credentials.password = ""; // No screenshots or logs ever contain it.
  serverVersions = await evaluate("(()=>{const value=JSON.parse(document.querySelector('#erp-bootstrap').textContent);return {registry_version:value.registry_version,asset_version:value.asset_version}})()");
  done("real browser form login and shell");
  phase = "keyboard";
  await evaluate("document.activeElement?.blur();document.body.tabIndex=-1;document.body.focus()");
  await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  check(await evaluate("Boolean(document.activeElement?.matches('[data-skip-content]'))"), "Keyboard skip link is not first");
  await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r", unmodifiedText: "\r" });
  await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
  check(await evaluate("document.activeElement?.id==='erp-content'"), "Keyboard skip link failed");
  await shot("keyboard-content-focus"); done("Tab and Enter reach skip link and content");
  if (!args.includes("--notes-installed")) {
  phase = "initial-settings";
  await nav("Settings"); await wait("Boolean(document.querySelector('select option[value=global]'))");
  await wait("document.querySelector('[data-client-content]').textContent.includes('No settings are available for this scope.')");
  const cleanWrites = requests.filter(item => item.path.endsWith("/erp_write_values")).length;
  await click("Save", "document.querySelector('[data-client-content]')");
  await wait(`Boolean(${button("Save", "document.querySelector('[data-client-content]')")})`);
  check(requests.filter(item => item.path.endsWith("/erp_write_values")).length === cleanWrites, "Clean Settings issued a write");
  await responsive("settings-empty"); done("initial empty declarations and clean-save no write");
  phase = "native-install";
  await nav("Addons"); await wait("Boolean(document.querySelector('.addon-card'))");
  const card = `[...document.querySelectorAll('.addon-card')].find(node=>node.textContent.includes('Notes'))`;
  await wait(`Boolean(${card}?.querySelector('button'))`);
  await evaluate(`${card}.querySelector('button').click()`);
  await wait("Boolean(document.querySelector('dialog[open]'))");
  await click("Install", "document.querySelector('dialog[open]')");
  await wait("[...document.querySelectorAll('[data-client-navigation] button')].some(node=>node.textContent==='Notes')");
  await wait(`Boolean(${card}?.textContent.includes('installed'))`);
  check(requests.some(item => item.path === "/erp/module/install"), "UI did not send native installation");
  done("native Notes installation through Addons UI and reload");
  }
  phase = "settings-save";
  await nav("Settings"); const setting = 'input[name="acceptance_notes.enabled"]';
  await wait(`Boolean(document.querySelector(${JSON.stringify(setting)}))`);
  settingsBaseline = await evaluate(`document.querySelector(${JSON.stringify(setting)}).checked`);
  await evaluate(`document.querySelector(${JSON.stringify(setting)}).click()`);
  await saveNotice("Settings saved.");
  await navigate();
  await wait(`Boolean(document.querySelector(${JSON.stringify(setting)}))`);
  check(await evaluate(`document.querySelector(${JSON.stringify(setting)}).checked`) === !settingsBaseline, "Settings did not persist after reload");
  await responsive("settings-stored");
  await evaluate(`document.querySelector(${JSON.stringify(setting)}).click()`);
  await saveNotice("Settings saved.");
  done("Settings save/reload persistence and original value restored");
  phase = "notes-create";
  await nav("Notes"); await wait("Boolean(document.querySelector('table[aria-label=Records]'))");
  await click("New", "document.querySelector('[data-client-content]')");
  await wait(`Boolean(document.querySelector(${JSON.stringify(field)}))`);
  const title = "Browser acceptance " + new Date().toISOString(), updated = title + " updated";
  await fill(field, title); await wait(`document.querySelector(${JSON.stringify(field)})?.disabled===false`);
  await saveNotice("Record saved.");
  recordId = await evaluate("new URL(location.href).searchParams.get('id')"); check(recordId && /^\d+$/.test(recordId), "Created note has no persisted ID");
  done("generic note create");
  phase = "notes-update";
  await fill(field, updated); await wait(`document.querySelector(${JSON.stringify(field)})?.disabled===false`);
  await saveNotice("Record saved.");
  await navigate(); await wait(`document.querySelector(${JSON.stringify(field)})?.value===${JSON.stringify(updated)}`);
  await responsive("note-persisted"); done("generic update/read after reload and addon widget");
  phase = "notes-delete";
  await click("Delete", "document.querySelector('[data-client-content]')");
  await wait("Boolean(document.querySelector('dialog[open]'))"); await click("Delete", "document.querySelector('dialog[open]')");
  await wait("Boolean(document.querySelector('input[aria-label=\"Search records\"]'))");
  await fill('input[aria-label="Search records"]', updated); await evaluate("document.querySelector('input[aria-label=\"Search records\"]').form.requestSubmit()");
  await wait("document.querySelector('[data-client-content]').textContent.includes('No matching records')"); done("generic delete and absent search result");
  phase = "logout";
  await click("Sign out"); await wait("Boolean(document.querySelector('input[name=password]'))");
  await navigate(credentials.origin + "/erp");
  await wait("Boolean(document.querySelector('input[name=password]'))"); done("logout and protected shell redirected to login");
  phase = "denial";
  const denied = await evaluate("fetch('/erp/json/2/acceptance_notes.note/search_count',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({domain:[]})}).then(response=>response.status)");
  check(denied === 401 || denied === 403, "Logged-out JSON call was not denied"); done("logged-out browser RPC denied");
  const unexpected = httpFailures.filter(item => item.phase !== "denial" && !["/favicon.ico"].includes(item.path));
  check(exceptionCount === 0 && unexpected.length === 0, "Unexpected browser exception or HTTP error; inspect private result counts");
  phase = "complete";
} catch {
  if (shot && session) { try { await shot("failure-redacted"); } catch { /* screenshot failure cannot replace original phase */ } }
} finally {
  const result = { functional: phase === "complete" ? "PASS" : "FAIL", failed_phase: phase === "complete" ? null : phase, failed_check: failedCheck, time_utc: new Date().toISOString(), host: hostname(), node: process.version, runner_source_revision: source, runner_source_dirty: dirty, runner_sha256: runnerHash, server_versions: serverVersions, browser, steps, screenshots, own_test_record_id: recordId, settings_restoration: steps.some(item => item.check.includes("original value restored")) ? "completed" : "unverified", exception_count: exceptionCount, console_error_count: consoleErrorCount, http_failures: httpFailures, requests, visual: "INCONCLUSIVE_NO_BASELINE: inspect redacted desktop/tablet/mobile/keyboard screenshots", accessibility: "Keyboard skip link only; axe and screen-reader audit not run", performance: "DEFERRED_PERFORMANCE; no product timing or vitals gate" };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n", { mode: 0o600 });
  console.log(JSON.stringify({ functional: result.functional, failed_phase: result.failed_phase, completed_checks: steps.length, screenshots: screenshots.length }));
  for (const request of pending.values()) clearTimeout(request.timer);
  socket?.close();
  if (chrome.pid && chrome.exitCode === null) {
    chrome.kill("SIGTERM");
    await Promise.race([once(chrome, "exit"), new Promise(resolve => setTimeout(resolve, 3000))]);
    if (chrome.exitCode === null) { chrome.kill("SIGKILL"); await Promise.race([once(chrome, "exit"), new Promise(resolve => setTimeout(resolve, 3000))]); }
  }
  await rm(profile, { recursive: true, force: true });
  if (phase !== "complete") process.exitCode = 1;
}

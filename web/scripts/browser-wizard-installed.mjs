/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Real installed-addon acceptance. Invoked while wasm_addon_poc's isolated server is alive:
// ERP_WIZARD_BROWSER_DRIVER=<absolute path to this file> (native or WASM fixture bridge).
// No routes, interception, synthetic RPC results, retries, traces or credential-bearing logs.
import assert from "node:assert/strict";
import { constants } from "node:fs";
import { open, mkdir, readFile, realpath, stat, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { homedir, hostname } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const configPath = process.argv[2];
if (process.argv.length !== 3 || !configPath) throw new Error("Supply one private fixture configuration path.");
const file = await open(configPath, constants.O_RDONLY | constants.O_NOFOLLOW);
let config;
try {
  const info = await file.stat();
  assert.ok(info.isFile() && (info.mode & 0o777) === 0o600 && info.uid === process.getuid(), "Fixture configuration must be an operator-owned mode-600 regular file.");
  assert.ok(info.size > 0 && info.size <= 16384, "Fixture configuration must be bounded.");
  try { config = JSON.parse(await file.readFile("utf8")); }
  catch { throw new Error("Fixture configuration must be valid JSON."); }
} finally { await file.close(); }
assert.ok(config !== null && typeof config === "object" && !Array.isArray(config), "Expected fixture configuration object.");
const configKeys = new Set([
  "schema", "fixtureKind", "url", "database", "login", "password", "deniedLogin", "deniedPassword",
  "bookModel", "wizardModel", "loanModel", "bookTitle", "bookId", "borrowerId", "borrowerName",
  "openMethod", "confirmMethod", "sourceRevision", "assetsRevision", "assetsManifestSha256", "deniedError",
]);
assert.ok(Object.keys(config).every(key => configKeys.has(key)), "Unknown fixture configuration key.");
assert.equal(config.schema, 1, "Unsupported fixture configuration schema.");
assert.ok(config.fixtureKind === "native" || config.fixtureKind === "wasm", "Fixture kind must be native or wasm.");
for (const key of ["url", "database", "login", "password", "deniedLogin", "deniedPassword", "bookModel", "wizardModel", "loanModel", "bookTitle", "borrowerName", "openMethod", "confirmMethod", "sourceRevision", "assetsRevision", "assetsManifestSha256"]) {
  assert.ok(typeof config[key] === "string" && config[key].length > 0 && config[key].length <= 1024, `Invalid configuration field: ${key}`);
}
for (const key of ["bookId", "borrowerId"]) assert.ok(Number.isSafeInteger(config[key]) && config[key] > 0, `Invalid fixture id: ${key}`);
for (const key of ["bookModel", "wizardModel", "loanModel"]) assert.match(config[key], /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/);
for (const key of ["openMethod", "confirmMethod"]) assert.match(config[key], /^[a-z][a-z0-9_]*$/);
for (const key of ["sourceRevision", "assetsRevision"]) assert.match(config[key], /^[0-9a-f]{40}$/);
assert.match(config.assetsManifestSha256, /^[0-9a-f]{64}$/);
assert.ok(config.deniedError !== null && typeof config.deniedError === "object" && !Array.isArray(config.deniedError), "Expected exact denial contract.");
assert.ok(Object.keys(config.deniedError).every(key => ["status", "name", "message"].includes(key)), "Unknown denial contract key.");
assert.equal(config.deniedError.status, 403);
assert.equal(config.deniedError.name, "erp.exceptions.AccessError");
assert.ok(typeof config.deniedError.message === "string" && config.deniedError.message.length > 0 && config.deniedError.message.length <= 4096, "Expected bounded complete denial message.");
const origin = new URL(config.url);
assert.ok(origin.protocol === "http:" && origin.hostname === "127.0.0.1" && origin.origin === config.url, "The fixture must be an isolated loopback HTTP origin.");
const output = process.env.CLIENT_BROWSER_OUTPUT ?? path.join(path.dirname(configPath), "browser-results");
await mkdir(output, { mode: 0o700, recursive: true });
const outputInfo = await stat(output);
assert.ok(outputInfo.isDirectory() && outputInfo.uid === process.getuid() && (outputInfo.mode & 0o077) === 0, "Browser output must be an operator-owned private directory.");

// resolve only the operator's private installation, never workspace or /tmp tooling.
const qaRoot = path.join(homedir(), ".local/share/erpforall-qa");
const qaInfo = await stat(qaRoot);
assert.ok(qaInfo.isDirectory() && qaInfo.uid === process.getuid() && (qaInfo.mode & 0o777) === 0o700, "Playwright tooling directory must be operator-owned mode 700.");
const playwrightPath = await realpath(path.join(qaRoot, "node_modules/playwright/index.mjs"));
assert.ok(playwrightPath.startsWith((await realpath(qaRoot)) + path.sep), "Playwright must resolve inside the operator-owned QA directory.");
const { chromium } = await import(pathToFileURL(playwrightPath).href);
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");
const result = {
  status: "FAIL", fixture_kind: config.fixtureKind,
  scope: `real installed ${config.fixtureKind} lending addon, real HTTP server and compiled client; isolated test database`,
  host: hostname(), time_utc: new Date().toISOString(), node: process.version,
  command: "node web/scripts/browser-wizard-installed.mjs <private-fixture-config>",
  source_revision: config.sourceRevision, assets_revision: config.assetsRevision,
  assets_manifest_sha256: config.assetsManifestSha256,
  driver_sha256: sha256(await readFile(fileURLToPath(import.meta.url))),
  checks: [], rpc: [], served_assets: [], page_errors: [],
  gaps: ["Removed transient-row UI recovery is not covered by this driver; see backend acceptance.", "No vacuum, expiry or cross-node claim; this run covers only the recorded fixture kind."],
};
if (config.fixtureKind === "wasm") {
  const packageRevision = process.env.ERP_WIZARD_PACKAGE_REVISION;
  const packageSha256 = process.env.ERP_WIZARD_PACKAGE_SHA256;
  assert.match(packageRevision ?? "", /^[0-9a-f]{40}$/, "Expected component package producer revision.");
  assert.match(packageSha256 ?? "", /^[0-9a-f]{64}$/, "Expected component package SHA256.");
  const packageDirectory = process.env.ERP_POC_PACKAGES;
  assert.ok(packageDirectory, "Expected installed component package directory.");
  assert.equal(sha256(await readFile(path.join(packageDirectory, "sample_library_wasm.erpaddon"))), packageSha256, "Component package differs from its producer pin.");
  result.package_revision = packageRevision;
  result.package_sha256 = packageSha256;
}
// Failure output may contain locator values. Remove private fixture credentials everywhere.
const redact = text => [config.password, config.deniedPassword, config.database, config.login, config.deniedLogin]
  .reduce((value, secret) => value.replaceAll(secret, "[redacted]"), String(text));
let browser, page, authenticated = false, phase = "startup";
const assetsDirectory = process.env.ERP_WIZARD_ASSETS;
assert.ok(assetsDirectory, "ERP_WIZARD_ASSETS must identify the compiled asset directory.");
const manifestBytes = await readFile(path.join(assetsDirectory, "manifest.json"));
assert.equal(sha256(manifestBytes), config.assetsManifestSha256, "Asset manifest differs from the bridge pin.");
let manifest;
try { manifest = JSON.parse(manifestBytes.toString("utf8")); }
catch { throw new Error("Compiled asset manifest is invalid JSON."); }
assert.ok(Array.isArray(manifest.assets), "Expected compiled manifest assets.");
try {
  browser = await chromium.launch({ headless: true, ...(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {}) });
  result.browser = browser.version();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  context.setDefaultTimeout(15000);
  context.setDefaultNavigationTimeout(30000);
  page = await context.newPage();
  page.on("pageerror", error => result.page_errors.push(redact(error.message)));
  // Observe request metadata only; never record login bodies, headers or cookies.
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.origin === config.url && url.pathname.startsWith("/erp/json/2/")) {
      result.rpc.push({ phase, path: url.pathname, method: request.method() });
    }
  });
  const rpc = async (target, model, method, args) => {
    const answer = await target.evaluate(async ({ model, method, args }) => {
      const response = await fetch(`/erp/json/2/${model}/${method}`, {
        method: "POST", credentials: "same-origin", redirect: "error", cache: "no-store",
        headers: { "Content-Type": "application/json", Accept: "application/json" }, body: JSON.stringify(args),
      });
      return { status: response.status, value: await response.json() };
    }, { model, method, args });
    assert.equal(answer.status, 200, `Backend read failed: ${model}/${method}`);
    return answer.value;
  };
  const login = async (target, loginName, password, internal = true) => {
    await target.goto(config.url + "/erp/login?db=" + encodeURIComponent(config.database));
    await target.locator('input[name="login"]').fill(loginName);
    await target.locator('input[name="password"]').fill(password);
    await target.locator('button[type="submit"]').click();
    if (internal) await target.locator("[data-client-navigation]").waitFor();
    else await target.waitForURL(config.url + "/erp/login_successful");
  };
  const books = async () => await rpc(page, config.bookModel, "web_read", {
    ids: [config.bookId], specification: { title: {}, available: {}, times_lent: {}, last_borrower: {} }, context: {},
  });
  const loans = async () => {
    const answer = await rpc(page, config.loanModel, "web_search_read", {
      domain: [["book_id", "=", config.bookId]], specification: { book_id: {}, borrower_id: {}, due_date: {}, state: {} },
      offset: 0, limit: 20, order: "id", context: {},
    });
    assert.ok(Array.isArray(answer.records) && Number.isSafeInteger(answer.length), "Expected real loan search result.");
    assert.equal(answer.records.length, answer.length, "Fixture loan readback must be complete.");
    return answer.records;
  };
  phase = "login";
  await login(page, config.login, config.password);
  authenticated = true;
  result.checks.push({ scenario: "ordinary internal user authenticates through real login form", passed: true });
  // Hash what was actually served, cross-checking the private fixture's manifest pin.
  const assetUrls = await page.locator('script[src],link[rel="stylesheet"][href]').evaluateAll(nodes => nodes.map(node => node.src || node.href));
  for (const url of assetUrls) {
    const parsed = new URL(url);
    if (parsed.origin !== config.url || !parsed.pathname.startsWith("/erp/assets/")) continue;
    const response = await context.request.get(url);
    assert.equal(response.status(), 200, "Loaded compiled asset must be served successfully.");
    const hash = sha256(await response.body());
    const pinned = manifest.assets.find(asset => "/erp/assets/" + asset.path === parsed.pathname);
    assert.ok(pinned, "Served asset is absent from the pinned manifest.");
    assert.equal(hash, pinned.sha256, "Served asset differs from the pinned manifest.");
    result.served_assets.push({ path: parsed.pathname, sha256: hash });
  }
  assert.ok(result.served_assets.some(asset => asset.path.endsWith(".js")), "Expected compiled client JavaScript asset.");
  assert.ok(result.served_assets.some(asset => asset.path.endsWith(".css")), "Expected compiled client stylesheet asset.");
  phase = "caller";
  const menus = await rpc(page, "ir.ui.menu", "erp_load_web_menus", { context: {} });
  assert.ok(Array.isArray(menus.nodes), "Expected installed menus.");
  let bookAction;
  for (const node of menus.nodes) {
    if (node.action_id === null) continue;
    const action = await rpc(page, "ir.actions.act_window", "erp_load_web_action", { ids: [node.action_id], context: {} });
    if (action.model === config.bookModel) { bookAction = node.action_id; break; }
  }
  assert.ok(Number.isSafeInteger(bookAction), "Installed book menu action is missing.");
  const callerUrl = `${config.url}/erp/action-${bookAction}?id=${config.bookId}&view_type=form`;
  await page.goto(callerUrl);
  const launch = page.locator('[data-client-content]').getByRole("button", { name: "Lend", exact: true });
  await launch.waitFor({ state: "visible" });
  const dialog = page.locator(".wizard-dialog");
  const methodPath = `/erp/json/2/${config.wizardModel}/${config.confirmMethod}`;
  const savePath = `/erp/json/2/${config.wizardModel}/web_save`;
  const open = async () => {
    const actionResponse = page.waitForResponse(response => new URL(response.url()).pathname === `/erp/json/2/${config.bookModel}/${config.openMethod}`);
    await launch.click();
    const response = await actionResponse;
    assert.equal(response.status(), 200);
    const action = await response.json();
    await dialog.waitFor({ state: "visible" });
    assert.equal(await dialog.count(), 1);
    assert.equal(page.url(), callerUrl, "Opening a modal must preserve the caller URL.");
    assert.equal(action.context.default_book_id, config.bookId);
    // The sample's book is read-only since 1.3.2 (set by the Lend button) and renders as a link, not an input.
    const bookInput = dialog.locator('input[name="book_id"]');
    const bookShown = await bookInput.count()
      ? await bookInput.inputValue()
      : (await dialog.locator(".many2one-readonly").first().innerText()).trim();
    assert.equal(bookShown, config.bookTitle);
    assert.match(action.context.default_due_date, /^\d{4}-\d{2}-\d{2}$/);
    assert.equal(await dialog.locator('input[name="due_date"]').inputValue(), action.context.default_due_date);
    assert.equal(await dialog.locator('input[name="borrower_id"]').inputValue(), "");
    return action.context.default_due_date;
  };
  const editDueDate = async value => {
    // Arm before filling: date inputs can emit change during fill rather than on blur.
    // Escape during the resulting request is deliberately guarded by WizardDialog;
    // this case tests dismissal of an idle draft, after the field has also blurred.
    const [response] = await Promise.all([
      page.waitForResponse(answer => new URL(answer.url()).pathname === `/erp/json/2/${config.wizardModel}/onchange`),
      (async () => {
        await dialog.locator('input[name="due_date"]').fill(value);
        await dialog.locator('input[name="due_date"]').press("Tab");
      })(),
    ]);
    assert.equal(response.status(), 200, "Date onchange must succeed before dismissal or confirmation.");
    await page.waitForFunction(() => document.querySelector(".wizard-dialog")?.getAttribute("aria-busy") === "false");
    assert.equal(await dialog.locator('input[name="due_date"]').inputValue(), value);
  };
  phase = "cancel";
  const beforeBook = await books(), beforeLoans = await loans();
  const statistic = page.locator(".form-stat-button .form-stat-value");
  await page.waitForFunction(expected => document.querySelector(".form-stat-button .form-stat-value")?.textContent === String(expected), beforeLoans.length);
  assert.equal(await statistic.textContent(), String(beforeLoans.length));
  result.checks.push({scenario:"installed relation count equals actual authorized loan readback before lending",passed:true});

  for (const dismiss of ["button", "escape"]) {
    const start = result.rpc.length;
    await open();
    // Unsaved edits must also be discarded without business writes or a confirmation prompt.
    await editDueDate("2099-12-20");
    if (dismiss === "button") await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
    else await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "detached" });
    assert.equal(await page.locator(".choice-dialog").count(), 0);
    assert.equal(page.url(), callerUrl);
    assert.ok(!result.rpc.slice(start).some(call => call.path === savePath || call.path === methodPath), "Cancellation must not save or execute confirm.");
    assert.deepEqual(await books(), beforeBook, "Cancellation changed the persisted book.");
    assert.deepEqual(await loans(), beforeLoans, "Cancellation changed persisted loans.");
    result.checks.push({ scenario: `${dismiss} cancellation discards edits without save, confirm or business mutation`, passed: true });
  }
  phase = "confirm";
  await open();
  const due = "2099-12-21";
  const borrower = dialog.locator('input[name="borrower_id"]');
  await borrower.fill(config.borrowerName);
  await dialog.locator('[role="listbox"][id^="editor-autocomplete-"]').getByRole("option", { name: config.borrowerName, exact: true }).click();
  await editDueDate(due);
  await dialog.getByRole("button", { name: "Lend", exact: true }).waitFor({ state: "visible" });
  await page.screenshot({ path: path.join(output, "wizard-before-confirm.png") });
  const start = result.rpc.length;
  const confirmResponse = page.waitForResponse(response => new URL(response.url()).pathname === methodPath);
  await dialog.getByRole("button", { name: "Lend", exact: true }).click();
  const confirmed = await confirmResponse;
  assert.equal(confirmed.status(), 200);
  const notification = await confirmed.json();
  assert.equal(notification.tag, "display_notification");
  assert.equal(notification.params.next.type, "ir.actions.act_window_close");
  await dialog.waitFor({ state: "detached" });
  const notice = page.locator("[data-client-notice]");
  await notice.filter({ hasText: notification.params.message }).waitFor({ state: "visible" });
  assert.equal(await notice.textContent(), `${notification.params.title}: ${notification.params.message}`);
  await page.getByRole("button", { name: "Return", exact: true }).waitFor({ state: "visible" });
  // Completion and its caller read have settled; no fixed sleeps or successful-rerun policy.
  await page.waitForLoadState("networkidle");
  const calls = result.rpc.slice(start);
  assert.equal(calls.filter(call => call.path === savePath).length, 1, "Confirm must save the wizard exactly once.");
  assert.equal(calls.filter(call => call.path === methodPath).length, 1, "Confirm method must execute exactly once.");
  assert.ok(calls.findIndex(call => call.path === savePath) < calls.findIndex(call => call.path === methodPath), "Wizard save must precede confirm.");
  assert.equal(calls.filter(call => call.path === `/erp/json/2/${config.bookModel}/web_read`).length, 1, "Caller must reload exactly once.");
  assert.equal(page.url(), callerUrl);
  const afterLoans = await loans();
  assert.equal(afterLoans.length, beforeLoans.length + 1);
  const created = afterLoans.filter(loan => !beforeLoans.some(prior => prior.id === loan.id));
  assert.equal(created.length, 1);
  // Relational web projections contain an object with id, not a label-only assertion.
  const relationId = value => Number.isSafeInteger(value) ? value : value?.id;
  assert.equal(relationId(created[0].book_id), config.bookId);
  assert.equal(relationId(created[0].borrower_id), config.borrowerId);
  assert.equal(created[0].due_date, due);
  assert.equal(created[0].state, "out");
  const afterBook = await books();
  assert.equal(afterBook[0].available, false);
  assert.equal(afterBook[0].times_lent, beforeBook[0].times_lent + 1);
  assert.equal(afterBook[0].last_borrower, config.borrowerName);
  result.checks.push({ scenario: "real modal defaults, selected borrower and edited due date persist in one loan; notification closes, caller reloads once and route is unchanged", passed: true, loan_id: created[0].id, due_date: due });
  await page.waitForFunction(expected => document.querySelector(".form-stat-button .form-stat-value")?.textContent === String(expected), afterLoans.length);
  assert.equal(await statistic.textContent(), String(afterLoans.length));
  await page.locator(".form-stat-button").click();
  await page.locator(".list-table tbody tr").first().waitFor();
  assert.ok(page.url().includes("method_action="), "Related list must keep its dynamic action scope.");
  assert.ok((await page.locator(".list-table").textContent()).includes(config.borrowerName), "Related list must show the actual saved loan.");
  await page.goBack(); await launch.waitFor({state:"visible"});
  await page.waitForFunction(expected => document.querySelector(".form-stat-button .form-stat-value")?.textContent === String(expected), afterLoans.length);
  for(const width of [320,1440]) {
    await page.setViewportSize({width,height:900});
    const geometry=await page.locator(".form-stat-button").evaluate(button=>{
      const value=button.querySelector(".form-stat-value").getBoundingClientRect(),label=button.querySelector(".form-stat-label").getBoundingClientRect(),box=button.getBoundingClientRect();
      return {height:box.height,width:box.width,valueWidth:value.width,labelWidth:label.width,labelTop:label.top,valueBottom:value.bottom};
    });
    assert.ok(geometry.height>=54 && geometry.valueWidth<=geometry.width && geometry.labelWidth<=geometry.width && geometry.labelTop>=geometry.valueBottom, "Shared statistic layout must keep count/label aligned without overflow.");
    await page.screenshot({path:path.join(output,`relation-count-${width}.png`)});
    result.checks.push({scenario:"shared relation-count sizing and baseline",width,geometry,passed:true});
  }
  result.checks.push({scenario:"real count refresh after lending; smart action opens saved related loan and history return recounts",passed:true});
  await page.screenshot({ path: path.join(output, "caller-after-confirm.png") });
  phase = "denied";
  const deniedContext = await browser.newContext();
  const deniedPage = await deniedContext.newPage();
  deniedPage.setDefaultTimeout(15000);
  await login(deniedPage, config.deniedLogin, config.deniedPassword, false);
  const denied = await deniedPage.evaluate(async ({ model, method, bookId }) => {
    const response = await fetch(`/erp/json/2/${model}/${method}`, { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ids: [bookId] }) });
    return { status: response.status, body: await response.json() };
  }, { model: config.bookModel, method: config.openMethod, bookId: config.bookId });
  assert.equal(denied.status, config.deniedError.status);
  assert.equal(denied.body.name, config.deniedError.name);
  assert.equal(denied.body.message, config.deniedError.message);
  assert.deepEqual(await books(), afterBook);
  assert.deepEqual(await loans(), afterLoans);
  result.checks.push({ scenario: "portal user's real browser RPC is denied before opening wizard, with no business mutation (not modal error UI coverage)", passed: true, status: denied.status, error_name: denied.body.name, message: redact(denied.body.message) });
  await deniedContext.close();
  assert.deepEqual(result.page_errors, []);
  result.status = "PASS";
} catch (error) {
  result.failure = { phase, message: redact(error instanceof Error ? error.message : error) };
  if (authenticated && page && !page.isClosed()) {
    try {
      // Never capture the sign-in form. Identity controls are masked even after login.
      if (await page.locator('input[name="password"],input[name="login"]').count() === 0) {
        const screenshot = "failure.png";
        await page.screenshot({ path: path.join(output, screenshot), timeout: 10000, mask: [page.locator(".identity,[data-client-identity]")] });
        result.failure.screenshot = screenshot;
      }
    } catch (screenshotError) {
      result.failure.screenshot_error = redact(screenshotError instanceof Error ? screenshotError.message : screenshotError);
    }
  }
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  const raw = JSON.stringify(result, null, 2) + "\n";
  await writeFile(path.join(output, "result.json"), raw, { mode: 0o600 });
  process.stdout.write(raw);
}

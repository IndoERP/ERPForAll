/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled reusable file-editor/dropzone evidence with a deferred caller callback.
// This fixture does not exercise an upload endpoint, binary field or attachment storage.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { homedir, hostname } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { auditPage } from "../testing/axe.ts";
import { preparePrivateOutputDirectory } from "./browser-erp-ui-states.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.FILE_EDITORS_BROWSER_OUTPUT ?? process.argv[2];
assert.ok(output, "Set FILE_EDITORS_BROWSER_OUTPUT or pass a task-owned output directory.");
await preparePrivateOutputDirectory(output);
const hash = value => createHash("sha256").update(value).digest("hex");
const client = name => JSON.stringify(path.join(root, "addons/web/static/src/client", name));
const write = async (name, contents) => {
  const destination = path.join(output, name);
  await mkdir(path.dirname(destination), { recursive: true, mode: 0o700 });
  await writeFile(destination, contents, { mode: 0o600 }); await chmod(destination, 0o600);
};
const source = `
import { createFileEditor } from ${client("file-editor.ts")};
import { createDropzone } from ${client("dropzone.ts")};
import { MessageServices } from ${client("messages.ts")};
import { ClientError } from ${client("transport.ts")};
const scene = document.querySelector('#scene');
let editor, dropzone, messages, owner, current, represented, completions, transportCalls, reports;
const report = error => reports.push({ name: error instanceof Error ? error.name : 'unknown', kind: error?.kind, message: error instanceof Error ? error.message : String(error) });
function dispose() {
  owner?.abort(); dropzone?.dispose(); editor?.dispose(); messages?.dispose();
  dropzone = editor = messages = owner = undefined; scene.replaceChildren();
}
function mount(readonly = false) {
  dispose(); reports = []; represented = 'previous fixture value'; completions = 0; transportCalls = 0; current = undefined;
  owner = new AbortController(); messages = new MessageServices(scene, report);
  editor = createFileEditor({ label: 'Supporting file', accept: '.txt,text/plain', readonly,
    limits: { maxFiles: 1, maxFileBytes: 1048576, maxTotalBytes: 1048576 }, messages, signal: owner.signal, report,
    transport: (files, context) => {
      transportCalls++;
      return new Promise((resolve, reject) => { current = { files, context, resolve, reject }; });
    },
    onComplete: result => { represented = result; completions++; },
  });
  scene.append(editor.node);
  dropzone = createDropzone({ target: editor.node, signal: owner.signal,
    disabled: () => readonly || editor.node.dataset.filePending === 'true',
    onDrop: event => { void editor.select(Array.from(event.dataTransfer?.files ?? [])).catch(report); },
  });
  document.body.append(dropzone.node);
}
window.fileFixture = {
  mount, dispose,
  progress: (loaded, total) => { if (!current) throw new Error('No fixture transport is pending'); current.context.progress(loaded, total); },
  complete: () => { if (!current) throw new Error('No fixture transport is pending'); current.resolve('prepared fixture value'); },
  fail: () => { if (!current) throw new Error('No fixture transport is pending'); current.reject(new ClientError('network', 'Fixture upload connection failed.')); },
  snapshot: () => ({ represented, completions, transportCalls, reports, aborted: current?.context.signal.aborted ?? false,
    pending: editor?.node.dataset.filePending, state: editor?.node.dataset.state,
    files: current?.files.map(file => ({ name: file.name, size: file.size, type: file.type })) ?? [],
  }),
  refresh: () => dropzone.refresh(),
};
mount(); window.fixtureReady = true;
`;
const bundled = await build({ stdin: { contents: source, resolveDir: root, sourcefile: "file-editors-browser-fixture.ts", loader: "ts" },
  bundle: true, write: false, format: "esm", platform: "browser", target: "es2023", metafile: true,
  alias: { "@erpforall/elang": path.join(root, "web/src/index.ts"), "@erpforall/expr": path.join(root, "web/packages/expr/src/index.ts") },
});
assert.equal(bundled.outputFiles.length, 1);
const javascript = bundled.outputFiles[0].contents;
const css = await readFile(path.join(root, "addons/web/static/src/client/client.css"));
const sourceFiles = [];
for (const file of Object.keys(bundled.metafile.inputs).sort()) {
  if (file === "file-editors-browser-fixture.ts") continue;
  const absolute = path.resolve(root, file);
  sourceFiles.push({ path: path.relative(root, absolute), sha256: hash(await readFile(absolute)) });
}
for (const file of ["web/scripts/browser-file-editors.mjs", "web/testing/axe.ts", "web/pnpm-lock.yaml", "addons/web/static/src/client/client.css"]) {
  sourceFiles.push({ path: file, sha256: hash(await readFile(path.join(root, file))) });
}
await write("fixture-entry.ts", source); await write("fixture.js", javascript); await write("client.css", css);
let revision;
try { revision = (await readFile(path.join(root, "REVISION"), "utf8")).trim(); }
catch (error) {
  if (error.code !== "ENOENT") throw error;
  revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
}
assert.match(revision, /^[0-9a-f]{40}(?:-dirty)?$/, "Source revision must identify the actual checkout or sync copy");
const qaRequire = createRequire(path.join(homedir(), ".local/share/erpforall-qa/package.json"));
const { chromium } = qaRequire("playwright");
const playwrightVersion = qaRequire("playwright/package.json").version;
const viewports = [
  { name: "desktop", width: 1440, height: 1000 },
  { name: "intermediate", width: 820, height: 1000 },
  { name: "phone", width: 390, height: 844 },
];
const manifest = { schema: 1, status: "FAIL", task: "a later release", generated_at: new Date().toISOString(),
  command: process.argv, host: hostname(), node: process.version, playwright: playwrightVersion, revision,
  browser_transport: "Playwright Chromium", sourceFiles, fixture_entry_sha256: hash(source),
  javascript_sha256: hash(javascript), css_sha256: hash(css), appearances: ["light", "dark"], viewports,
  boundary: "Compiled reusable client editor/dropzone with trusted callback fixtures. Filechooser/setInputFiles and browser DragEvent/DataTransfer are exercised; no OS drag, endpoint, binary field, authentication, attachment authorization, persistence, scanning, screen-reader speech or deployment acceptance.",
  entries: [], failures: [], console_errors: [], interactions: [],
};
let browser, page, server;
const snapshot = () => page.evaluate(() => window.fileFixture.snapshot());
const waitState = state => page.waitForFunction(value => document.querySelector('.file-editor')?.dataset.state === value, state);
const mount = async (readonly = false) => { await page.evaluate(value => window.fileFixture.mount(value), readonly); };
const file = (name = "report.txt") => ({ name, mimeType: "text/plain", buffer: Buffer.from("abcdefghij") });
const select = async name => {
  await page.locator('.file-editor input[type="file"]').setInputFiles(file(name));
  await waitState("pending");
};
async function geometry() {
  return page.evaluate(() => {
    const target = document.querySelector('.file-editor').getBoundingClientRect();
    const overlay = document.querySelector('.file-dropzone').getBoundingClientRect();
    return { target: target.toJSON(), overlay: overlay.toJSON(), documentWidth: document.documentElement.scrollWidth, viewportWidth: innerWidth };
  });
}
function aligned(value) {
  for (const key of ["x", "y", "width", "height"]) assert.ok(Math.abs(value.target[key] - value.overlay[key]) <= 1, `Target and dropzone ${key} differ: ${JSON.stringify(value)}`);
  assert.ok(value.target.width > 0 && value.target.height > 0, "Target must have real rendered geometry");
}
async function capture(state, appearance, viewport) {
  const name = `${appearance}/${viewport.name}/${state}`, screenshot = `${name}.png`, auditFile = `${name}.axe.json`;
  const observed = await snapshot();
  const bounds = await page.evaluate(() => ({ documentWidth: document.documentElement.scrollWidth, viewportWidth: innerWidth,
    controls: [...document.querySelectorAll('.file-editor button:not(:disabled)')].filter(node => !node.hidden).map(node => {
      const r = node.getBoundingClientRect(); return { label: node.textContent, x: r.x, width: r.width, height: r.height };
    }),
  }));
  assert.ok(bounds.documentWidth <= bounds.viewportWidth + 1, `No horizontal overflow: ${JSON.stringify(bounds)}`);
  for (const control of bounds.controls) {
    assert.ok(control.width >= 24 && control.height >= 24, `Readable control target: ${JSON.stringify(control)}`);
    assert.ok(control.x >= -1 && control.x + control.width <= viewport.width + 1, `Control fits viewport: ${JSON.stringify(control)}`);
  }
  await mkdir(path.dirname(path.join(output, screenshot)), { recursive: true, mode: 0o700 });
  await page.screenshot({ path: path.join(output, screenshot), fullPage: true }); await chmod(path.join(output, screenshot), 0o600);
  let audit;
  try { audit = await auditPage(page); }
  catch (error) {
    const raw = await page.evaluate(async () => window.axe ? window.axe.run(document) : { error: "axe engine was unavailable" });
    await write(auditFile, JSON.stringify(raw, null, 2) + "\n"); throw error;
  }
  await write(auditFile, JSON.stringify(audit, null, 2) + "\n");
  assert.deepEqual(audit.violations, [], `Zero axe violations required for ${name}`);
  manifest.entries.push({ state, appearance, viewport: viewport.name, screenshot,
    sha256: hash(await readFile(path.join(output, screenshot))), axe: auditFile, axe_violations: audit.violations.length, bounds, observed });
}

try {
  server = createServer((request, response) => {
    response.setHeader("Cache-Control", "no-store");
    if (request.method !== "GET") { response.writeHead(405); response.end("Fixture has no upload endpoint"); return; }
    if (request.url === "/fixture.js") { response.setHeader("Content-Type", "text/javascript; charset=utf-8"); response.end(javascript); return; }
    if (request.url === "/client.css") { response.setHeader("Content-Type", "text/css; charset=utf-8"); response.end(css); return; }
    if (request.url === "/favicon.ico") { response.writeHead(204); response.end(); return; }
    if (request.url !== "/") { response.writeHead(404); response.end("Unknown fixture route"); return; }
    response.setHeader("Content-Type", "text/html; charset=utf-8");
    response.end('<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>File editor behavior fixture</title><link rel="stylesheet" href="/client.css"><style>main{max-width:1000px;margin:auto;padding:24px;box-sizing:border-box}h1{font-size:24px}#scene{margin-top:24px;min-width:0}#tail{height:1000px}</style></head><body><main><h1>File editor behavior fixture</h1><p>Reusable client controls with an explicit caller-owned transport fixture.</p><div id="scene"></div><button id="other-control" type="button">Other control</button><div id="tail" aria-hidden="true"></div></main><script type="module" src="/fixture.js"></script></body></html>');
  });
  await new Promise((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
  const origin = `http://127.0.0.1:${server.address().port}`; manifest.origin = origin;
  browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME_BIN,
    args: ["--no-sandbox", "--disable-setuid-sandbox", "--disable-dev-shm-usage"],
  });
  manifest.browser = browser.version();
  page = await browser.newPage(); page.setDefaultTimeout(5000); page.setDefaultNavigationTimeout(15000);
  page.on("pageerror", error => manifest.console_errors.push(error.message));
  page.on("console", message => { if (message.type() === "error") manifest.console_errors.push(message.text()); });
  for (const appearance of manifest.appearances) for (const viewport of viewports) {
    await page.setViewportSize({ width: viewport.width, height: viewport.height });
    await page.emulateMedia({ colorScheme: appearance, reducedMotion: "no-preference" });
    await page.goto(origin); await page.waitForFunction(() => window.fixtureReady === true);
    await page.evaluate(value => { document.documentElement.dataset.theme = value; }, appearance);
    try {
      await capture("empty", appearance, viewport);
      const chooserOpened = page.waitForEvent("filechooser");
      await page.locator('[data-file-choose]').click();
      await (await chooserOpened).setFiles(file()); await waitState("pending");
      assert.equal((await snapshot()).transportCalls, 1, "Native chooser must start one callback operation");
      await page.evaluate(() => window.fileFixture.progress(3)); await waitState("loading");
      assert.equal(await page.locator('.file-editor progress').getAttribute("value"), null, "Unknown totals remain indeterminate");
      await capture("indeterminate", appearance, viewport);
      await page.evaluate(() => window.fileFixture.progress(5, 10));
      assert.equal(await page.locator('.file-editor progress').getAttribute("value"), "5");
      assert.equal(await page.locator('.file-editor progress').getAttribute("max"), "10");
      await capture("progress", appearance, viewport);
      await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");
      assert.equal((await snapshot()).represented, "prepared fixture value");
      assert.equal(await page.locator('.file-editor input[type="file"]').inputValue(), "", "Successful native selection is cleared");
      await capture("completed", appearance, viewport);
      await select(); await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");
      assert.equal((await snapshot()).completions, 2, "Selecting the same file again completes again");

      await mount(); await select(); await page.locator('[data-file-cancel]').focus(); await page.keyboard.press("Enter");
      const confirmation = page.getByRole("dialog", { name: "Cancel file operation" }); await confirmation.waitFor();
      await capture("cancel-confirmation", appearance, viewport);
      await confirmation.getByRole("button", { name: "Keep uploading", exact: true }).focus(); await page.keyboard.press("Enter");
      await confirmation.waitFor({ state: "detached" });
      assert.equal(await page.evaluate(() => document.activeElement?.hasAttribute('data-file-cancel')), true, "Declining cancellation restores opener focus");
      assert.equal((await snapshot()).aborted, false);
      await page.keyboard.press("Enter"); await confirmation.waitFor(); await page.keyboard.press("Escape");
      await confirmation.waitFor({ state: "detached" }); assert.equal((await snapshot()).aborted, false);
      assert.equal(await page.evaluate(() => document.activeElement?.hasAttribute('data-file-cancel')), true, "Escape restores opener focus");
      await page.locator('[data-file-cancel]').focus(); await page.keyboard.press("Enter"); await confirmation.waitFor();
      await confirmation.getByRole("button", { name: "Cancel upload", exact: true }).focus(); await page.keyboard.press("Enter");
      await waitState("aborted");
      const cancelled = await snapshot();
      assert.equal(cancelled.aborted, true); assert.equal(cancelled.completions, 0); assert.equal(cancelled.transportCalls, 1);
      assert.equal(cancelled.represented, "previous fixture value");
      assert.equal(await page.locator('.file-editor input[type="file"]').inputValue(), "", "Cancellation clears native selection for same-file retry");
      assert.equal(await page.evaluate(() => document.activeElement?.hasAttribute('data-file-choose')), true, "Confirmed cancellation focuses surviving Choose control");
      await capture("cancelled", appearance, viewport);
      // This fixture deliberately ignores AbortSignal. Late transport success must not commit UI output.
      await page.evaluate(() => window.fileFixture.complete());
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      assert.equal((await snapshot()).completions, 0);
      const retryChooser = page.waitForEvent("filechooser"); await page.locator('[data-file-choose]').click();
      await (await retryChooser).setFiles(file()); await waitState("pending");
      assert.equal((await snapshot()).transportCalls, 2, "Same-file retry after cancellation starts exactly one callback");
      await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");

      await mount(); await select(); await page.evaluate(() => window.fileFixture.fail()); await waitState("error");
      const failed = await snapshot(); assert.equal(failed.represented, "previous fixture value"); assert.equal(failed.completions, 0);
      assert.equal(failed.transportCalls, 1); assert.ok(failed.reports.some(error => error.kind === "network" && error.message === "Fixture upload connection failed."));
      assert.equal(await page.locator('.client-notification[data-level="danger"]').count(), 1);
      await capture("failed", appearance, viewport);
      assert.equal(await page.locator('.file-editor input[type="file"]').inputValue(), "", "Failure clears native selection for same-file retry");
      const failureRetry = page.waitForEvent("filechooser"); await page.locator('[data-file-choose]').click();
      await (await failureRetry).setFiles(file()); await waitState("pending");
      assert.equal((await snapshot()).transportCalls, 2, "Same-file retry after failure starts exactly one callback");
      await page.locator('#other-control').focus(); await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");
      assert.equal(await page.locator('#other-control').evaluate(node => node === document.activeElement), true, "Completion does not steal unrelated focus");
      await mount(); await select(); await page.locator('[data-file-cancel]').focus(); await page.keyboard.press("Enter");
      await confirmation.waitFor(); await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");
      await confirmation.waitFor({ state: "detached" });
      assert.equal(await page.evaluate(() => document.activeElement?.hasAttribute('data-file-choose')), true, "Completion with confirmation open focuses Choose");
      await capture("completion-with-confirmation", appearance, viewport);

      await mount(); await select(`${"very-long-file-name-".repeat(12)}.txt`);
      await page.evaluate(() => window.fileFixture.progress(7, 10)); await capture("long-filename", appearance, viewport);
      await page.evaluate(() => window.fileFixture.complete()); await waitState("completed");
      await mount(true); assert.equal(await page.locator('[data-file-choose]').isDisabled(), true);
      assert.equal(await page.locator('.file-editor input[type="file"]').isDisabled(), true); await capture("readonly", appearance, viewport);

      await mount();
      const transfer = await page.evaluateHandle(() => { const data = new DataTransfer(); data.items.add(new File(['abcdefghij'], 'dropped.txt', { type: 'text/plain' })); return data; });
      await page.locator('.file-editor').dispatchEvent("dragenter", { dataTransfer: transfer });
      assert.equal(await page.locator('.file-dropzone').getAttribute("data-dragging"), "true"); aligned(await geometry());
      await capture("drag-over", appearance, viewport);
      await page.evaluate(() => window.scrollTo(0, 80));
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      const afterScroll = await geometry(); aligned(afterScroll);
      await page.evaluate(() => { document.querySelector('#scene').style.paddingTop = '18px'; window.fileFixture.refresh(); });
      const afterPatch = await geometry(); aligned(afterPatch);
      await page.locator('.file-dropzone').dispatchEvent("drop", { dataTransfer: transfer }); await waitState("pending");
      assert.equal((await snapshot()).transportCalls, 1); assert.equal((await snapshot()).files[0].name, "dropped.txt");
      assert.equal(await page.locator('.file-dropzone').getAttribute("data-dragging"), "false");
      await transfer.dispose();
      await page.evaluate(() => { window.fileFixture.complete(); window.scrollTo(0, 0); document.querySelector('#scene').style.paddingTop = ''; });
      await waitState("completed");
      manifest.interactions.push({ appearance, viewport: viewport.name, nativeChooser: true, sameFileReselection: true,
        keyboardConfirmation: true, declineAndEscapeFocus: true, cancellationSuppressesLateSuccess: true,
        failedCallbackPreservesValue: true, sameFileRecoveryAfterFailureAndCancel: true, terminalFocusReturn: true, unrelatedFocusPreserved: true, readonly: true, dropTransportCalls: 1, afterScroll, afterPatch });
    } catch (error) {
      manifest.failures.push({ appearance, viewport: viewport.name, message: error instanceof Error ? error.stack : String(error) });
      const name = `${appearance}/${viewport.name}/failure`;
      await mkdir(path.dirname(path.join(output, `${name}.png`)), { recursive: true, mode: 0o700 });
      await page.screenshot({ path: path.join(output, `${name}.png`), fullPage: true }); await chmod(path.join(output, `${name}.png`), 0o600);
      await write(`${name}.dom.json`, JSON.stringify(await page.evaluate(() => ({ activeElement: document.activeElement?.outerHTML,
        scene: document.querySelector('#scene')?.outerHTML, fixture: window.fileFixture.snapshot(), geometry: [...document.querySelectorAll('dialog,button,.file-editor,.file-dropzone')].map(node => ({ tag: node.tagName, text: node.textContent, rectangle: node.getBoundingClientRect().toJSON() })) })), null, 2) + "\n");
      await write("partial-manifest.json", JSON.stringify(manifest, null, 2) + "\n");
    }
  }
  await mount(); await page.emulateMedia({ reducedMotion: "reduce" }); await select();
  await page.evaluate(() => window.fileFixture.progress(4, 10));
  const reduced = await page.locator('.file-editor progress').evaluate(node => ({ matched: matchMedia('(prefers-reduced-motion: reduce)').matches,
    animation: getComputedStyle(node).animationName, transitionDuration: getComputedStyle(node).transitionDuration,
    state: node.closest('.file-editor').dataset.state, value: node.getAttribute('value'), max: node.getAttribute('max') }));
  assert.equal(reduced.matched, true); assert.equal(reduced.value, "4"); assert.equal(reduced.max, "10");
  assert.equal(reduced.state, "loading"); assert.equal(reduced.animation, "none");
  await capture("reduced-motion-progress", "dark", viewports.at(-1)); manifest.reduced_motion = reduced;
  assert.deepEqual(manifest.console_errors, [], "No unexpected browser console or page errors");
  assert.deepEqual(manifest.failures, [], "File editor browser checks failed; inspect the manifest and raw audits");
  manifest.status = "PASS";
} catch (error) {
  manifest.error = error instanceof Error ? error.stack : String(error); process.exitCode = 1;
} finally {
  const cleanupErrors = [];
  if (page) { try { await page.evaluate(() => window.fileFixture?.dispose()); } catch (error) { cleanupErrors.push(String(error)); } }
  if (browser) { try { await browser.close(); } catch (error) { cleanupErrors.push(String(error)); } }
  if (server) { try { server.closeAllConnections(); await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())); } catch (error) { cleanupErrors.push(String(error)); } }
  if (cleanupErrors.length) { manifest.status = "FAIL"; manifest.cleanup_errors = cleanupErrors; process.exitCode = 1; }
  await write("manifest.json", JSON.stringify(manifest, null, 2) + "\n");
  console.log(JSON.stringify({ status: manifest.status, host: manifest.host, revision, entries: manifest.entries.length, failures: manifest.failures.length, output }));
}

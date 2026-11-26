/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The document viewer in a real browser (Playwright Chromium), under the shell's own Content-Security-Policy.
// Scope: the viewer component (document-viewer.ts with the bundled PDF renderer), the dialog service and the
// client stylesheet, served by a fixture that answers the attachment route. Not the whole client, no backend.
//
//   cd web && CLIENT_BROWSER_OUTPUT=<empty directory> node scripts/browser-document-viewer.mjs [extra.pdf]
//
// Writes result.json (host, time and versions written here, never by hand) and screenshots to the directory.
import assert from "node:assert/strict";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { hostname } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { crc32, deflateSync } from "node:zlib";
import { build, version as esbuildVersion } from "esbuild";
import { chromium } from "playwright";

const root = fileURLToPath(new URL("../../", import.meta.url));
const client = path.join(root, "addons/web/static/src/client");
const output = process.env.CLIENT_BROWSER_OUTPUT;
if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to an isolated output directory.");
await mkdir(output, { recursive: true });

// The shell's policy, read from the server source so the check cannot drift from what is served.
const assetsSource = await readFile(path.join(root, "crates/erp-server/src/assets.rs"), "utf8");
const policyLine = /"(default-src 'self'; script-src \{script_src\};[^"]+)"/.exec(assetsSource);
if (!policyLine) throw new Error("The shell policy was not found in crates/erp-server/src/assets.rs.");
const shellPolicy = (authority) => policyLine[1].replace("{script_src}", "'self'").replace("{authority}", authority);
const contentSource = await readFile(path.join(root, "crates/erp-server/src/web/content.rs"), "utf8");
const attachmentPolicy = /const ATTACHMENT_CSP: &str = "([^"]+)";/.exec(contentSource)?.[1];
if (!attachmentPolicy) throw new Error("The attachment policy was not found in crates/erp-server/src/web/content.rs.");

/** A PDF of `pages` A4 pages, each with one line of text in a standard font the file does not embed. */
function samplePdf(pages) {
  const objects = [];
  const add = (body) => { objects.push(body); return objects.length; };
  const font = add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
  const pagesId = objects.length + 1 + pages * 2;
  const kids = [];
  for (let number = 1; number <= pages; number++) {
    const text = `BT /F1 36 Tf 72 720 Td (Fixture page ${number} of ${pages}) Tj ET\n0.1 0.4 0.4 rg 72 500 300 120 re f`;
    const content = add(`<< /Length ${text.length} >>\nstream\n${text}\nendstream`);
    kids.push(add(`<< /Type /Page /Parent ${pagesId} 0 R /MediaBox [0 0 595 842] /Contents ${content} 0 R /Resources << /Font << /F1 ${font} 0 R >> >> >>`));
  }
  assert.equal(add(`<< /Type /Pages /Count ${pages} /Kids [${kids.map((id) => `${id} 0 R`).join(" ")}] >>`), pagesId);
  const catalog = add(`<< /Type /Catalog /Pages ${pagesId} 0 R >>`);
  let file = "%PDF-1.4\n";
  const offsets = objects.map((body, at) => { const offset = file.length; file += `${at + 1} 0 obj\n${body}\nendobj\n`; return offset; });
  const xref = file.length;
  file += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n${offsets.map((offset) => `${String(offset).padStart(10, "0")} 00000 n \n`).join("")}`;
  file += `trailer\n<< /Size ${objects.length + 1} /Root ${catalog} 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(file, "latin1");
}
/** A PNG of one colour, written here so the fixture carries no binary file. */
function samplePng(width, height, rgb) {
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type, "latin1"), data]), out = Buffer.alloc(12 + data.length);
    out.writeUInt32BE(data.length, 0); body.copy(out, 4); out.writeUInt32BE(crc32(body) >>> 0, 8 + data.length);
    return out;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0); header.writeUInt32BE(height, 4); header[8] = 8; header[9] = 2; // 8 bits per channel, RGB
  const row = Buffer.concat([Buffer.from([0]), ...Array.from({ length: width }, () => Buffer.from(rgb))]);
  const pixels = Buffer.concat(Array.from({ length: height }, () => row));
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", header), chunk("IDAT", deflateSync(pixels)), chunk("IEND", Buffer.alloc(0))]);
}
const extraPath = process.argv[2];
const files = new Map([
  ["/erp/content/1", { type: "application/pdf", data: samplePdf(7), name: "fixture-seven-pages.pdf" }],
  ["/erp/content/2", { type: "image/png", data: samplePng(2, 1, [8, 109, 113]), name: "fixture.png" }],
  ...(extraPath ? [["/erp/content/4", { type: "application/pdf", data: await readFile(extraPath), name: path.basename(extraPath) }]] : []),
]);

const entry = `
import { DialogService } from "./dialog-service.js";
import { attachmentViewerFile, openDocumentViewer } from "./document-viewer.js";
import { RpcClient } from "./transport.js";
const errors = [];
const dialogs = new DialogService(error => errors.push(String(error)));
const rpc = new RpcClient((input, init) => fetch(input, init), () => {});
window.viewerFixture = {
  errors,
  open(specs, index) {
    document.getElementById("opener").focus();
    openDocumentViewer({ rpc, dialogs, report: error => errors.push(String(error)) }, specs.map(spec => attachmentViewerFile(spec)), index ?? 0);
  },
};
document.documentElement.dataset.ready = "1";
`;
const bundle = await build({ stdin: { contents: entry, resolveDir: client, sourcefile: "document-viewer-fixture.ts", loader: "ts" }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023", minify: true, logLevel: "silent" });
const script = Buffer.from(bundle.outputFiles[0].contents);
const stylesheet = await readFile(path.join(client, "client.css"));

const requests = [];
const server = createServer((req, res) => {
  const pathname = new URL(req.url, "http://fixture").pathname;
  requests.push(`${req.method} ${req.url}`);
  const authority = req.headers.host;
  if (pathname === "/") {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Content-Security-Policy": shellPolicy(authority), "X-Content-Type-Options": "nosniff" });
    res.end(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/client.css"><title>Document viewer fixture</title><main><button id="opener" type="button">Open the document</button></main><script type="module" src="/fixture.js"></script>`);
    return;
  }
  if (pathname === "/client.css") { res.writeHead(200, { "Content-Type": "text/css; charset=utf-8" }); res.end(stylesheet); return; }
  if (pathname === "/fixture.js") { res.writeHead(200, { "Content-Type": "text/javascript; charset=utf-8" }); res.end(script); return; }
  const file = files.get(pathname);
  if (file) {
    // The headers of the attachment route: its own policy, nosniff, inline (or a download when asked).
    const download = new URL(req.url, "http://fixture").searchParams.get("download") === "true";
    res.writeHead(200, { "Content-Type": file.type, "Content-Security-Policy": attachmentPolicy, "X-Content-Type-Options": "nosniff", "Content-Disposition": `${download ? "attachment" : "inline"}; filename="${file.name}"` });
    res.end(file.data);
    return;
  }
  if (req.method === "POST" && pathname === "/erp/json/2/ir.config.setting/erp_read_values") {
    req.resume();
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ wire_major: 1, values: [{ key: "web.document_viewer_max_pages", value: 2000 }, { key: "web.document_viewer_print_max_pages", value: 100 }] }));
    return;
  }
  if (pathname.startsWith("/erp/content/")) { res.writeHead(404, { "Content-Type": "text/plain; charset=utf-8" }); res.end("Not found."); return; }
  res.writeHead(404); res.end();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;

const checks = [], consoleErrors = [], consoleWarnings = [], pageErrors = [], violations = [], screenshots = [];
const result = {
  status: "FAIL", scope: "document viewer component with the bundled PDF renderer under the shell policy; fixture answers for the attachment route; not the whole client, no backend",
  host: hostname(), time_utc: new Date().toISOString(), node: process.version, esbuild: esbuildVersion, shell_policy: shellPolicy("127.0.0.1"), attachment_policy: attachmentPolicy,
  fixture_script_bytes: script.length, extra_document: extraPath ? path.basename(extraPath) : null,
};
let activePage;
const browser = await chromium.launch(process.env.CHROME_BIN ? { executablePath: process.env.CHROME_BIN } : {});
result.chromium = browser.version();
try {
  const check = async (name, run) => { await run(); checks.push(name); };
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  const page = await context.newPage();
  activePage = page;
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("console", (message) => { if (message.type() === "error") consoleErrors.push(message.text()); else if (message.type() === "warning") consoleWarnings.push(message.text()); });
  await page.exposeFunction("reportViolation", (text) => { violations.push(text); });
  await page.addInitScript(() => { document.addEventListener("securitypolicyviolation", (event) => { void window.reportViolation(`${event.violatedDirective} blocked ${event.blockedURI}`); }); });
  await page.goto(origin + "/");
  await page.waitForSelector("html[data-ready='1']");
  const open = (specs, index) => page.evaluate(([list, at]) => window.viewerFixture.open(list, at), [specs, index ?? 0]);
  const pdf = { id: 1, name: "fixture-seven-pages.pdf", mimetype: "application/pdf" };
  const picture = { id: 2, name: "fixture.png", mimetype: "image/png" };
  const shot = async (name) => { const file = `${name}.png`; await page.screenshot({ path: path.join(output, file) }); screenshots.push(file); };
  /** Pixels of the page's canvas that are not white: a drawn page has text and a filled rectangle. */
  const ink = (number) => page.evaluate((at) => {
    const canvas = document.querySelector(`.document-viewer-page[data-page="${at}"] canvas`);
    if (!canvas) return -1;
    const data = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data;
    let dark = 0;
    for (let i = 0; i < data.length; i += 4) if (data[i] < 200 || data[i + 1] < 200 || data[i + 2] < 200) dark++;
    return dark;
  }, number);
  const text = (selector) => page.locator(selector).first().textContent();

  await check("a PDF opens in a modal dialog and its pages are drawn", async () => {
    await open([pdf, picture]);
    await page.waitForSelector(".document-viewer[open] .document-viewer-page[data-page='1'] canvas");
    assert.equal(await text(".document-viewer-page-total"), "of 7");
    assert.equal(await page.locator(".document-viewer-page-input").inputValue(), "1");
    assert.equal(await text(".document-viewer-name"), "fixture-seven-pages.pdf");
    assert.ok(await ink(1) > 500, `page 1 has ${await ink(1)} drawn pixels`);
    assert.equal(await page.evaluate(() => document.querySelector(".document-viewer").matches(":modal")), true);
    assert.ok(await page.evaluate(() => document.querySelector(".document-viewer").contains(document.activeElement)), "focus is inside the viewer");
    const box = await page.locator(".document-viewer").boundingBox();
    assert.deepEqual([box.x, box.y, box.width, box.height], [0, 0, 1440, 900]);
    await shot("pdf-1440-light");
  });
  await check("the opening zoom fits the width but stays at or under 125 %", async () => {
    assert.equal(await text(".document-viewer-zoom"), "125%");
  });
  await check("Next page, the Page Down key and the page box move and draw", async () => {
    await page.getByRole("button", { name: "Next page" }).click();
    assert.equal(await page.locator(".document-viewer-page-input").inputValue(), "2");
    await page.locator(".document-viewer-stage").focus();
    await page.keyboard.press("PageDown");
    assert.equal(await page.locator(".document-viewer-page-input").inputValue(), "3");
    await page.locator(".document-viewer-page-input").fill("7");
    await page.keyboard.press("Enter");
    await page.waitForSelector(".document-viewer-page[data-page='7'] canvas");
    assert.ok(await ink(7) > 500);
    assert.equal(await page.getByRole("button", { name: "Next page" }).isDisabled(), true);
    // Far pages give their canvases back.
    assert.equal(await page.locator(".document-viewer-page[data-page='1'] canvas").count(), 0);
  });
  await check("scrolling the stage follows to the page in view", async () => {
    await page.locator(".document-viewer-stage").evaluate((stage) => { stage.scrollTop = 0; });
    await page.waitForFunction(() => document.querySelector(".document-viewer-page-input").value === "1");
    await page.waitForSelector(".document-viewer-page[data-page='1'] canvas");
  });
  await check("zoom, fit and rotate redraw the page", async () => {
    await page.getByRole("button", { name: "Zoom out" }).click();
    assert.equal(await text(".document-viewer-zoom"), "100%");
    const width = async () => Math.round((await page.locator(".document-viewer-page[data-page='1']").boundingBox()).width);
    assert.ok(Math.abs(await width() - 795) <= 2, `page width at 100 % is ${await width()}`);
    await page.getByRole("button", { name: "Fit page" }).click();
    const fitted = await page.locator(".document-viewer-page[data-page='1']").boundingBox();
    const stage = await page.locator(".document-viewer-stage").boundingBox();
    assert.ok(fitted.height <= stage.height, "the whole page shows");
    await page.keyboard.press("r");
    await page.waitForFunction(() => { const box = document.querySelector(".document-viewer-page[data-page='1']").getBoundingClientRect(); return box.width > box.height; });
    await page.waitForSelector(".document-viewer-page[data-page='1'] canvas");
    assert.ok(await ink(1) > 100);
    await page.keyboard.press("r"); await page.keyboard.press("r"); await page.keyboard.press("r");
    await page.getByRole("button", { name: "Fit width" }).click();
    assert.equal(await page.getByRole("button", { name: "Fit width" }).getAttribute("aria-pressed"), "true");
  });
  await check("the dark theme shows the same viewer", async () => {
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    await shot("pdf-1440-dark");
    await page.evaluate(() => { delete document.documentElement.dataset.theme; });
  });
  await check("the next file is the picture; the page controls go", async () => {
    await page.getByRole("button", { name: "Next file" }).click();
    await page.waitForFunction(() => document.querySelector(".document-viewer-image")?.naturalWidth === 2);
    assert.equal(await text(".document-viewer-count"), "2 of 2");
    assert.equal(await page.locator(".document-viewer-paging").isHidden(), true);
    assert.equal(await text(".document-viewer-zoom"), "Fit");
    await page.keyboard.press("ArrowRight");
    await page.waitForSelector(".document-viewer-page[data-page='1'] canvas");
  });
  await check("Print prepares every page and asks the browser to print", async () => {
    await page.evaluate(() => { window.printed = []; const frames = HTMLIFrameElement.prototype; const view = Object.getOwnPropertyDescriptor(frames, "contentWindow").get; Object.defineProperty(frames, "contentWindow", { configurable: true, get() { const inner = view.call(this); if (inner && !inner.printPatched) { inner.printPatched = true; inner.print = () => { window.printed.push([...inner.document.images].map((image) => image.naturalWidth)); inner.dispatchEvent(new Event("afterprint")); }; } return inner; } }); });
    await page.getByRole("button", { name: "Print" }).click();
    await page.waitForFunction(() => window.printed.length === 1);
    const widths = await page.evaluate(() => window.printed[0]);
    // 595 points at 150 dots per inch.
    assert.deepEqual(widths, Array.from({ length: 7 }, () => 1239));
    await page.waitForFunction(() => document.querySelector(".document-viewer-print-frame") === null);
  });
  await check("Escape closes the viewer and focus returns to the control that opened it", async () => {
    await page.keyboard.press("Escape");
    await page.waitForFunction(() => document.querySelector(".document-viewer") === null);
    assert.equal(await page.evaluate(() => document.activeElement?.id), "opener");
  });
  await check("a file the route refuses shows the failure with the download link", async () => {
    await open([{ id: 3, name: "secret.pdf", mimetype: "application/pdf" }]);
    await page.waitForSelector(".document-viewer-problem:not([hidden])");
    assert.equal(await text(".document-viewer-problem p"), "Not found.");
    assert.equal(await page.locator(".document-viewer-problem a").getAttribute("href"), "/erp/content/3?download=true");
    assert.equal(await page.locator(".document-viewer canvas").count(), 0);
    await shot("refused-1440-light");
    await page.getByRole("button", { name: "Close" }).click();
  });
  if (extraPath) {
    await check(`the document given on the command line draws (${path.basename(extraPath)})`, async () => {
      await open([{ id: 4, name: path.basename(extraPath), mimetype: "application/pdf" }]);
      await page.waitForSelector(".document-viewer-page[data-page='1'] canvas");
      result.extra_pages = await text(".document-viewer-page-total");
      result.extra_page_1_drawn_pixels = await ink(1);
      assert.ok(result.extra_page_1_drawn_pixels > 500);
      await shot("extra-1440-light");
      await page.keyboard.press("Escape");
      await page.waitForFunction(() => document.querySelector(".document-viewer") === null);
    });
  }
  await check("a phone: the viewer fills the screen and the controls are at least 44 pixels", async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    await open([pdf, picture]);
    await page.waitForSelector(".document-viewer-page[data-page='1'] canvas");
    const sizes = await page.locator(".document-viewer .document-viewer-btn").evaluateAll((nodes) => nodes.filter((node) => node.offsetParent !== null).map((node) => { const box = node.getBoundingClientRect(); return [Math.round(box.width), Math.round(box.height)]; }));
    assert.ok(sizes.length >= 10 && sizes.every(([width, height]) => width >= 44 && height >= 44), JSON.stringify(sizes));
    const paper = await page.locator(".document-viewer-page[data-page='1']").boundingBox();
    assert.ok(paper.x >= 0 && paper.x + paper.width <= 390, "the page fits the width of the phone");
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= 390), true);
    await shot("pdf-390-light");
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    await shot("pdf-390-dark");
    await page.keyboard.press("Escape");
  });
  await check("no error, no policy violation; the viewer asked for the attachment route and its settings only", async () => {
    const fixtureErrors = await page.evaluate(() => window.viewerFixture.errors);
    // A refused file is a 404 the browser logs itself; nothing else may be an error.
    const unexpected = consoleErrors.filter((line) => !/Failed to load resource: the server responded with a status of 404/.test(line));
    assert.deepEqual({ pageErrors, unexpected, violations, fixtureErrors }, { pageErrors: [], unexpected: [], violations: [], fixtureErrors: [] });
    const asked = [...new Set(requests.filter((line) => !/^GET \/(client\.css|fixture\.js|favicon\.ico)?$/.test(line)))];
    assert.ok(asked.every((line) => /^GET \/erp\/content\/\d+$/.test(line) || line === "POST /erp/json/2/ir.config.setting/erp_read_values"), JSON.stringify(asked));
  });
  result.status = "PASS";
} catch (error) {
  result.failure = error instanceof Error ? `${error.message}\n${error.stack}` : String(error);
  // What the viewer showed when the check failed: a picture and the viewer's own text.
  if (activePage) {
    await activePage.screenshot({ path: path.join(output, "failure.png") }).then(() => screenshots.push("failure.png"), () => {});
    result.failure_state = await activePage.evaluate(() => {
      const viewer = document.querySelector(".document-viewer");
      const shown = (selector) => { const node = viewer?.querySelector(selector); return node ? { hidden: node.hidden, text: node.textContent } : null; };
      return viewer ? { status: shown(".document-viewer-status"), problem: shown(".document-viewer-problem"), notice: shown(".document-viewer-notice"), total: shown(".document-viewer-page-total"), pages: viewer.querySelectorAll(".document-viewer-page").length, canvases: viewer.querySelectorAll("canvas").length, errors: window.viewerFixture?.errors } : "no viewer in the page";
    }).catch((problem) => String(problem));
  }
} finally {
  await browser.close();
  await new Promise((resolve) => server.close(resolve));
  Object.assign(result, { checks, screenshots, console_errors: consoleErrors, console_warnings: [...new Set(consoleWarnings)], page_errors: pageErrors, policy_violations: violations, requests: [...new Set(requests)] });
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify(result, null, 2));
}
if (result.status !== "PASS") process.exit(1);

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { chromium } from "playwright";

const root = new URL("../../", import.meta.url);
const [host, guest, pinnedDigest] = process.argv.slice(2);
if (!host || !guest || !pinnedDigest) throw new Error("Usage: browser-restricted-template.mjs HOST GUEST.wasm PINNED_SHA256");
const sha256 = source => createHash("sha256").update(source).digest("hex");
const arch = "res.partner.card";
function compile(source) {
  const frame = [Buffer.from("ERPUI2\n")];
  for (const value of ["op compile", "mode prod", `arch_id ${arch}`, `source_digest ${sha256(source)}`,
    "language en_US", 'host_schema {"fields":["name"]}', "compiler_policy 1", "file arch_data", `source ${source}`]) {
    const bytes = Buffer.from(value), length = Buffer.alloc(4);
    length.writeUInt32BE(bytes.length);
    frame.push(length, bytes);
  }
  const result = spawnSync(host, [guest, pinnedDigest], { input: Buffer.concat(frame), encoding: "utf8", timeout: 45_000 });
  if (result.error) throw result.error;
  return result;
}

const source = '<templates>\n<t t-name="card"><strong><field name="name"/></strong></t></templates>';
const ok = compile(source);
assert.equal(ok.status, 0, ok.stderr);
const refused = compile(source.replace('name="name"', 'name="undeclared"'));
assert.notEqual(refused.status, 0);
assert.match(refused.stderr, /res\.partner\.card:2:26:/);
assert.equal(refused.stdout, "");
const expected = { arch, sourceSha256: sha256(source), language: "en_US" };
const js = await build({
  stdin: { resolveDir: fileURLToPath(new URL("web/src/", root)), contents: `
    import { prepareRestrictedCard } from "./runtime/restricted-template.ts";
    document.addEventListener("securitypolicyviolation", () => document.body.dataset.violation = "1");
    const response = await fetch("/card.json");
    const render = prepareRestrictedCard(await response.text(), ${JSON.stringify(expected)});
    document.querySelector("main").append(render("card", JSON.stringify({name:{value:"<Company>"}}), document));
  ` }, bundle: true, format: "esm", write: false, target: "es2023",
});
const assets = await readFile(new URL("crates/erp-server/src/assets.rs", root), "utf8");
const policy = /"(default-src 'self'; script-src \{script_src\};[^"]+)"/.exec(assets)?.[1];
assert.ok(policy, "Shell content policy must resolve from the server");
const server = createServer((req, res) => {
  if (req.url === "/") {
    res.setHeader("Content-Security-Policy", policy.replace("{script_src}", "'self'").replace("{authority}", req.headers.host));
    res.setHeader("Content-Type", "text/html");
    res.end('<!doctype html><html lang="en"><title>Card</title><main></main><script type="module" src="/render.js"></script>');
  } else if (req.url === "/render.js") {
    res.setHeader("Content-Type", "text/javascript");
    res.end(js.outputFiles[0].contents);
  } else if (req.url === "/card.json") {
    res.setHeader("Content-Type", "application/json");
    res.end(ok.stdout);
  } else { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
let browser;
try {
  browser = await chromium.launch({ executablePath: process.env.CHROME_BIN, headless: true });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  assert.equal(await page.locator("main strong").innerText(), "<Company>");
  assert.equal(await page.locator("main strong *").count(), 0);
  assert.equal(await page.locator("body").getAttribute("data-violation"), null);
  process.stdout.write("WASI guest, original XML diagnostic and Chromium card rendering under shell CSP passed\n");
} finally {
  await browser?.close();
  await new Promise(resolve => server.close(resolve));
}

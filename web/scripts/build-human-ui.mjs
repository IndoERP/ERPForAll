/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Trusted XML is compiled ahead of publication. No template strings reach the browser compiler.
import { readFile, mkdir, writeFile, realpath } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { build } from "esbuild";
const root = fileURLToPath(new URL("../../", import.meta.url));
const args = process.argv.slice(2);
const options = new Map();
for (let i = 0; i < args.length; i += 2) {
  const key = args[i], value = args[i + 1];
  if (!["--addons", "--catalogue", "--extract-terms"].includes(key) || !value || value.startsWith("--") || options.has(key)) {
    throw new Error("Usage: build-human-ui.mjs [--addons web,addon] [--catalogue catalogue.json | --extract-terms output.json]");
  }
  options.set(key, value);
}
if (options.has("--catalogue") && options.has("--extract-terms")) throw new Error("Choose either extraction or localized compilation.");
const addons = (options.get("--addons") ?? "web").split(",");
if (!addons.includes("web") || new Set(addons).size !== addons.length || addons.some(name => !/^[a-z][a-z0-9_]*$/.test(name))) throw new Error("Invalid or duplicate addon selection");
const frames = [Buffer.from("ERPUI2\n")], entries = [], styles = [];
function frame(value) { const payload = Buffer.from(value), length = Buffer.alloc(4); length.writeUInt32BE(payload.length); frames.push(length, payload); }
frame(options.has("--extract-terms") ? "op extract" : "op compile"); frame("mode prod");
if (options.has("--catalogue")) {
  const catalogue = JSON.parse(await readFile(options.get("--catalogue"), "utf8"));
  if (typeof catalogue.language !== "string" || !Array.isArray(catalogue.terms)) throw new Error("A catalogue requires a language and a list of terms.");
  frame(`language ${catalogue.language}`);
  const compare = (left, right) => Buffer.compare(Buffer.from(left), Buffer.from(right));
  for (const term of catalogue.terms) {
    if (typeof term.addon !== "string" || typeof term.source !== "string" || typeof term.translation !== "string") throw new Error("Invalid catalogue term.");
  }
  for (const term of catalogue.terms.sort((a, b) => compare(a.addon, b.addon) || compare(a.source, b.source))) {
    frame(`term ${term.addon}`); frame(`original ${term.source}`); frame(`translated ${term.translation}`);
  }
}
for (const addon of addons) {
  const staticDir = await realpath(path.join(root, "addons", addon, "static"));
  const manifest = JSON.parse(await readFile(path.join(staticDir, "client.json"), "utf8"));
  if (manifest.schema !== 1 || typeof manifest.entry !== "string" || !Array.isArray(manifest.templates) || manifest.styles !== undefined && !Array.isArray(manifest.styles) || Object.keys(manifest).some(key => !["schema", "entry", "templates", "styles"].includes(key))) throw new Error(`Invalid client manifest: ${addon}`);
  async function sourcePath(relative, extension) {
    if (typeof relative !== "string" || relative.startsWith("/") || relative.split("/").some(part => !part || part === "." || part === "..") || !relative.endsWith(extension)) throw new Error(`Invalid addon source path: ${addon}`);
    const resolved = await realpath(path.join(staticDir, relative));
    if (!resolved.startsWith(staticDir + path.sep)) throw new Error(`Addon source escapes its static directory: ${addon}`);
    return resolved;
  }
  entries.push(await sourcePath(manifest.entry, ".ts"));
  for (const relative of manifest.templates) {
    const file = await sourcePath(relative, ".xml");
    frame("file addon_file"); frame(`addon ${addon}`); frame(`path ${relative}`); frame(`source ${await readFile(file, "utf8")}`);
  }
  for (const relative of manifest.styles ?? []) styles.push(await readFile(await sourcePath(relative, ".css")));
}
const compiler = process.env.ERP_UI_COMPILER;
if (!compiler) throw new Error("Set ERP_UI_COMPILER to the compiled erp-ui-compile executable.");
const compiled = spawnSync(compiler, [], { input: Buffer.concat(frames), maxBuffer: 16 * 1024 * 1024 });
if (compiled.error) throw compiled.error;
if (compiled.status !== 0) throw new Error(`Template compiler refused input: ${compiled.stderr.toString()}${compiled.stdout.toString()}`);
if (options.has("--extract-terms")) {
  const terms = JSON.parse(compiled.stdout.toString());
  await writeFile(options.get("--extract-terms"), JSON.stringify(terms, null, 2) + "\n");
  process.exit(0);
}
const bytes = compiled.stdout;
let cursor = 0;
function line() { const end = bytes.indexOf(10, cursor); if (end < 0) throw new Error("Truncated compiler frame"); const value = bytes.subarray(cursor, end).toString(); cursor = end + 1; return value; }
if (line() !== "erpforall-ui-compiled 1") throw new Error("Unknown compiler output");
const count = /^modules (\d+)$/.exec(line());
if (!count) throw new Error("Missing compiler module count");
const modules = new Map();
for (let i = 0; i < Number(count[1]); i++) {
  const frame = /^module (\S+) (\d+)$/.exec(line());
  if (!frame) throw new Error("Invalid compiler module frame");
  const end = cursor + Number(frame[2]);
  if (end > bytes.length || modules.has(frame[1])) throw new Error("Invalid compiler module length/name");
  modules.set(frame[1], bytes.subarray(cursor, end).toString()); cursor = end;
}
const diagnostics = bytes.subarray(cursor).toString();
const translationNotice = "warning info[skipped]: translation pass skipped: static text and .translate values are emitted untranslated until a later release\n";
if (diagnostics && diagnostics !== translationNotice) throw new Error(`Compiler diagnostics require review: ${diagnostics}`);
if (diagnostics) process.stderr.write(diagnostics); // Source-mode builds report their untranslated literals.
const templateEntry = [...modules.keys()].map((name, i) => `import * as t${i} from ${JSON.stringify(name)};`).join("\n") + `\nexport const templates = [${[...modules.keys()].map((_, i) => `t${i}`).join(",")}];`;
const bootEntry = entries.map(entry => `import ${JSON.stringify(entry)};`).join("\n") + `\nimport {boot} from ${JSON.stringify(path.join(root, "addons/web/static/src/client/index.ts"))}; void boot();`;
const output = await build({
  entryPoints: ["@erpforall/human-entry"], alias: { "@erpforall/client": path.join(root, "addons/web/static/src/client/public.ts") }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023", minify: true,
  plugins: [{ name: "trusted-elang-templates", setup(plugin) {
    plugin.onResolve({ filter: /^@erpforall\/human-entry$/ }, () => ({ path: "boot", namespace: "elang-compiled" }));
    plugin.onResolve({ filter: /^@erpforall\/human-templates$/ }, () => ({ path: "entry", namespace: "elang-compiled" }));
    plugin.onResolve({ filter: /.*/, namespace: "elang-compiled" }, args => modules.has(args.path) ? { path: args.path, namespace: "elang-compiled" } : null);
    plugin.onLoad({ filter: /.*/, namespace: "elang-compiled" }, args => ({ contents: args.path === "boot" ? bootEntry : args.path === "entry" ? templateEntry : modules.get(args.path), loader: "js", resolveDir: path.join(root, "web") }));
  } }],
});
// Browser acceptance fixtures isolate output; ordinary builds retain the standard destination.
const destination = process.env.ERP_UI_OUTPUT_DIR ?? path.join(root, "web/dist/human-ui");
if (!path.isAbsolute(destination)) throw new Error("ERP_UI_OUTPUT_DIR must be an absolute directory path.");
await mkdir(destination, { recursive: true });
const assets = [];
for (const [name, data, content_type] of [["client.js", output.outputFiles[0].contents, "text/javascript; charset=utf-8"], ["client.css", Buffer.concat(styles.flatMap(style => [style, Buffer.from("\n")])), "text/css; charset=utf-8"]]) {
  const sha256 = createHash("sha256").update(data).digest("hex"), extension = path.extname(name);
  const filename = `client.${sha256}${extension}`;
  await writeFile(path.join(destination, filename), data);
  assets.push({ name, path: filename, sha256, content_type });
}
await writeFile(path.join(destination, "manifest.json"), JSON.stringify({ schema: 1, assets }, null, 2) + "\n");
console.log(JSON.stringify({ destination, addons, templates: [...modules.keys()], assets }));

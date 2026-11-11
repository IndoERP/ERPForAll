#!/usr/bin/env node
/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Run only on an authorized build/benchmark host. No external page or browser profile is used.
// node scripts/measure-expr.mjs --output ../
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { EventEmitter, once } from "node:events";
import { mkdtemp, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { hostname, platform, release, arch, cpus, tmpdir, loadavg } from "node:os";
import { createServer } from "node:http";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { build, version as esbuildVersion } from "esbuild";
import { gunzipSync } from "node:zlib";
import { checkAdmissionRatchet } from "./expr-ratchet.ts";

const PINNED_CHROME = "Google Chrome 147.0.7727.55";
const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const root = resolve(web, "..");
const options = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  const key = process.argv[index];
  const value = process.argv[index + 1];
  if (!["--output", "--chrome", "--baseline"].includes(key) || value === undefined || options.has(key)) {
    throw new Error("usage: measure-expr.mjs --output FILE [--chrome EXECUTABLE] [--baseline JSON_OR_GZIP]");
  }
  options.set(key, value);
}
if (!options.has("--output")) throw new Error("--output is required; measurements must retain raw evidence");
const chromePath = options.get("--chrome") ?? "/usr/bin/google-chrome";
const chromeVersion = execFileSync(chromePath, ["--version"], { encoding: "utf8", timeout: 10000 }).trim();
if (chromeVersion !== PINNED_CHROME) throw new Error(`browser pin mismatch: expected ${PINNED_CHROME}; got ${chromeVersion}`);
const bundle = await build({ entryPoints: [resolve(web, "scripts/measure-expr-worker.ts")], bundle: true,
  write: false, format: "iife", target: "es2023", legalComments: "inline", logLevel: "silent" });
if (bundle.outputFiles.length !== 1) throw new Error("measurement worker must bundle to exactly one file");
const workerCode = bundle.outputFiles[0].text;
const profile = await mkdtemp(resolve(tmpdir(), "erp-uix-ast-chrome-"));
const chromeArguments = ["--headless=new", "--no-sandbox", "--disable-gpu", "--disable-background-networking",
  "--disable-component-update", "--disable-default-apps", "--disable-sync", "--metrics-recording-only",
  "--no-first-run", "--no-default-browser-check", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"];
const child = spawn(chromePath, chromeArguments, { stdio: ["ignore", "ignore", "pipe"] });
let childExited = false;
let childFailure;
child.once("exit", () => { childExited = true; });
child.once("error", (error) => { childFailure = error; });
let stderr = "";
child.stderr.on("data", (chunk) => { stderr = (stderr + chunk.toString()).slice(-65536); });
const server = createServer((request, response) => {
  if (request.method !== "GET" || request.url !== "/") {
    response.writeHead(404, { "Content-Type": "text/plain" });
    response.end("Not found");
    return;
  }
  response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Embedder-Policy": "require-corp",
    "Cross-Origin-Resource-Policy": "same-origin",
    "Cache-Control": "no-store",
  });
  response.end("<!doctype html><title>ERPForAll AST measurement</title>");
});
let socket;
let cdp;
const runDeadline = setTimeout(() => {
  console.error("browser measurement exceeded its 180-second bound");
  // Only the child launched above is signalled. Cleanup in finally closes its isolated profile.
  child.kill("SIGTERM");
  socket?.close();
}, 180000);

class Protocol extends EventEmitter {
  next = 0;
  pending = new Map();
  constructor(connection) {
    super();
    this.connection = connection;
    connection.addEventListener("message", ({ data }) => {
      const message = JSON.parse(data);
      if (message.id !== undefined) {
        const pending = this.pending.get(message.id);
        if (!pending) return;
        this.pending.delete(message.id);
        clearTimeout(pending.timer);
        if (message.error) pending.reject(new Error(`${pending.method}: ${JSON.stringify(message.error)}`));
        else pending.resolve(message.result);
      } else this.emit(message.method, message);
    });
    connection.addEventListener("close", () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error(`CDP connection closed during ${pending.method}`));
      }
      this.pending.clear();
    });
  }
  send(method, params = {}, sessionId) {
    const id = ++this.next;
    return new Promise((resolveResult, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP timeout: ${method}`));
      }, 30000);
      this.pending.set(id, { method, resolve: resolveResult, reject, timer });
      this.connection.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }
}

async function evaluate(expression, session) {
  const result = await cdp.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, session);
  if (result.exceptionDetails) throw new Error(`worker evaluation failed: ${JSON.stringify(result.exceptionDetails)}`);
  return result.result.value;
}
function boundedEvent(emitter, event, predicate, timeout = 10000) {
  return new Promise((resolveEvent, reject) => {
    const listener = (message) => {
      if (!predicate(message)) return;
      clearTimeout(timer); emitter.off(event, listener); resolveEvent(message);
    };
    const timer = setTimeout(() => { emitter.off(event, listener); reject(new Error(`timed out awaiting ${event}`)); }, timeout);
    emitter.on(event, listener);
  });
}
function samplingTotals(profile) {
  let estimatedAllocatedBytes = 0;
  let profileNodes = 0;
  const pending = [profile.head];
  while (pending.length) {
    const node = pending.pop();
    estimatedAllocatedBytes += node.selfSize;
    profileNodes++;
    for (const child of node.children) pending.push(child);
  }
  return { estimatedAllocatedBytes, profileNodes, sampleCount: profile.samples.length,
    interpretation: "statistical sampling estimate including collected objects; not exact bytes or allocation count" };
}
function sourceRevision() {
  // sync.sh supplies REVISION in copies without .git; never invent a revision.
  return readFile(resolve(root, "REVISION"), "utf8").then((text) => ({ source: "REVISION", value: text.trim() }), (error) => {
    if (error.code !== "ENOENT") throw error;
    const revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8", timeout: 10000 }).trim();
    const status = execFileSync("git", ["status", "--porcelain"], { cwd: root, encoding: "utf8", timeout: 10000 }).trim();
    return { source: "git", value: revision, dirty: status.length !== 0 };
  });
}

try {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("loopback measurement server has no TCP address");
  const pageUrl = `http://127.0.0.1:${address.port}/`;
  let endpoint;
  for (let attempt = 0; attempt < 100; attempt++) {
    if (childFailure) throw childFailure;
    if (childExited) throw new Error(`Chrome exited before CDP startup: ${stderr}`);
    const match = stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/);
    if (match) { endpoint = match[1]; break; }
    await delay(100);
  }
  if (!endpoint) throw new Error(`Chrome CDP startup timeout: ${stderr}`);
  socket = new WebSocket(endpoint);
  await Promise.race([once(socket, "open"), delay(10000).then(() => { throw new Error("CDP websocket open timeout"); })]);
  cdp = new Protocol(socket);
  const browser = await cdp.send("Browser.getVersion");
  const target = await cdp.send("Target.createTarget", { url: "about:blank" });
  const page = (await cdp.send("Target.attachToTarget", { targetId: target.targetId, flatten: true })).sessionId;
  await cdp.send("Page.enable", {}, page);
  const loaded = boundedEvent(cdp, "Page.loadEventFired", (event) => event.sessionId === page);
  const navigation = await cdp.send("Page.navigate", { url: pageUrl }, page);
  if (navigation.errorText) throw new Error(`loopback navigation failed: ${navigation.errorText}`);
  await loaded;
  if (!await evaluate("globalThis.crossOriginIsolated === true", page)) {
    throw new Error("loopback page did not become cross-origin isolated");
  }
  await cdp.send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, page);
  const attached = boundedEvent(cdp, "Target.attachedToTarget", (event) => event.params.targetInfo.type === "worker");
  await evaluate(`globalThis.exprWorker = new Worker(URL.createObjectURL(new Blob([${JSON.stringify(workerCode)}], {type:"text/javascript"}))); true`, page);
  const worker = (await attached).params.sessionId;
  await cdp.send("Runtime.enable", {}, worker);
  await cdp.send("HeapProfiler.enable", {}, worker);
  await cdp.send("Runtime.runIfWaitingForDebugger", {}, worker);
  let ready = false;
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await evaluate("typeof globalThis.exprMeasurement === 'object'", worker)) { ready = true; break; }
    await delay(25);
  }
  if (!ready) throw new Error("worker measurement API did not initialize");
  const workerEnvironment = await evaluate("globalThis.exprMeasurement.environment()", worker);
  if (!workerEnvironment.crossOriginIsolated) throw new Error("measurement worker must be cross-origin isolated");
  const fixtureDescriptions = await evaluate("globalThis.exprMeasurement.fixtures", worker);
  const paths = await evaluate("globalThis.exprMeasurement.paths", worker);
  const rows = [];
  for (let index = 0; index < fixtureDescriptions.length; index++) {
    for (const path of paths) {
      const args = `${index},${JSON.stringify(path)}`;
      const timing = await evaluate(`globalThis.exprMeasurement.benchmark(${args})`, worker);
      await evaluate("globalThis.exprMeasurement.release()", worker);
      await cdp.send("HeapProfiler.collectGarbage", {}, worker);
      const before = await cdp.send("Runtime.getHeapUsage", {}, worker);
      await cdp.send("HeapProfiler.startSampling", { samplingInterval: 1024, stackDepth: 64,
        includeObjectsCollectedByMajorGC: true, includeObjectsCollectedByMinorGC: true }, worker);
      await evaluate(`globalThis.exprMeasurement.workload(${args},256,false)`, worker);
      const sampled = (await cdp.send("HeapProfiler.stopSampling", {}, worker)).profile;
      await evaluate("globalThis.exprMeasurement.release()", worker);
      await cdp.send("HeapProfiler.collectGarbage", {}, worker);
      const beforeRetention = await cdp.send("Runtime.getHeapUsage", {}, worker);
      await evaluate(`globalThis.exprMeasurement.workload(${args},32,true)`, worker);
      await cdp.send("HeapProfiler.collectGarbage", {}, worker);
      const retained = await cdp.send("Runtime.getHeapUsage", {}, worker);
      await evaluate("globalThis.exprMeasurement.release()", worker);
      await cdp.send("HeapProfiler.collectGarbage", {}, worker);
      const released = await cdp.send("Runtime.getHeapUsage", {}, worker);
      rows.push({ ...timing, memory: { samplingInterval: 1024, sampledOperations: 256, retainedResults: 32,
        before, beforeRetention, retained, released,
        retainedUsedSizeDelta: retained.usedSize - beforeRetention.usedSize,
        releasedUsedSizeDelta: released.usedSize - beforeRetention.usedSize,
        samplingTotals: samplingTotals(sampled), samplingProfile: sampled } });
    }
  }
  const ratios = [];
  for (const fixture of fixtureDescriptions) {
    for (const [candidate, floor] of [["reader", "native_parse"], ["reader_decode", "native_parse_validate"]]) {
      const measured = rows.find((row) => row.fixture === fixture.name && row.path === candidate);
      const baseline = rows.find((row) => row.fixture === fixture.name && row.path === floor);
      if (!measured || !baseline) throw new Error("missing timing row");
      ratios.push({ fixture: fixture.name, candidate, floor,
        p50: baseline.p50 > 0 ? measured.p50 / baseline.p50 : null,
        p99: baseline.p99 > 0 ? measured.p99 / baseline.p99 : null,
        timerResolutionSufficient: baseline.p50 > 0 && baseline.p99 > 0,
        individualOperations: {
          p50: baseline.individualOperations.p50 > 0 ? measured.individualOperations.p50 / baseline.individualOperations.p50 : null,
          p99: baseline.individualOperations.p99 > 0 ? measured.individualOperations.p99 / baseline.individualOperations.p99 : null,
          timerResolutionSufficient: baseline.individualOperations.p50 > 0 && baseline.individualOperations.p99 > 0,
        } });
    }
  }
  const output = {
    schema: 1, capturedAt: new Date().toISOString(),
    command: { executable: process.execPath, argv: process.argv.slice(1), cwd: process.cwd() },
    host: { hostname: hostname(), platform: platform(), release: release(), arch: arch(), cpus: cpus(), loadavg: loadavg() },
    tools: { node: process.version, esbuild: esbuildVersion, chrome: chromeVersion, browser, chromeArguments },
    revision: await sourceRevision(), workerEnvironment, workerSha256: createHash("sha256").update(workerCode).digest("hex"),
    method: {
      isolation: "cross-origin isolated dedicated worker in owned headless Chrome profile; owned loopback HTML with COOP same-origin and COEP require-corp; no external page",
      timing: "cold first call, warmed batch-average p50/p99, and 512 single-operation samples; all raw samples including timer zeros preserved; sampling disabled during timing",
      gc: "Chrome default during timing; explicit HeapProfiler.collectGarbage at memory snapshot boundaries",
      floor: "valid unique-key JSON only; JSON.parse plus the exact same internal structural validator",
      counters: "reader plus decoder explicit application allocation sites and reader logical byte ledger; default budget construction included; not engine allocations",
      limitations: ["Sampled heap sizes are statistical estimates, not exact allocation counts or per-operation peak memory.",
        "Retained snapshots are isolate-wide and include engine/CDP metadata; signed deltas are not clamped.",
        "Batch p50/p99 describe per-operation averages. Individual p50/p99 include loop/retention harness; zero samples reflect timer resolution and are not removed. First calls may share initialized worker state.",
        "The native floor does not reject duplicate keys; fixtures have unique keys and are valid by construction.",
        "No browser physical peak bound is established by these snapshots. Fetch accumulation and error-path allocation sites are outside these successful text-input fixtures."],
    }, fixtures: fixtureDescriptions, rows, ratios,
  };
  const outputPath = resolve(options.get("--output"));
  await mkdir(dirname(outputPath), { recursive: true });
  await writeFile(outputPath, `${JSON.stringify(output, null, 2)}\n`);
  console.log(JSON.stringify({ output: outputPath, capturedAt: output.capturedAt, ratios }));
  if (options.has("--baseline")) {
    const name = options.get("--baseline");
    const bytes = await readFile(name);
    const baseline = JSON.parse((name.endsWith(".gz") ? gunzipSync(bytes) : bytes).toString("utf8"));
    const failures = checkAdmissionRatchet(baseline, output);
    if (failures.length) throw new Error(`AST measurement ratchet failed:\n${failures.join("\n")}`);
    console.log("AST measurement ratchet passed (no tolerance)");
  }
} finally {
  clearTimeout(runDeadline);
  try {
    if (cdp && socket?.readyState === WebSocket.OPEN) {
      try { await cdp.send("Browser.close"); }
      catch (error) { console.error(`Chrome close diagnostic: ${error.message}`); }
    }
    socket?.close();
    if (!childExited) {
      child.kill("SIGTERM");
      await Promise.race([once(child, "exit"), delay(5000)]);
    }
    if (!childExited) {
      child.kill("SIGKILL");
      await Promise.race([once(child, "exit"), delay(5000)]);
    }
    if (!childExited && !childFailure) throw new Error(`owned Chrome pid ${child.pid} did not exit; profile retained at ${profile}`);
    // Chrome subprocesses can finish profile writes just after the parent exits.
    // Retry only transient filesystem cleanup errors, within this owned directory.
    await rm(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  } finally {
    if (server.listening) {
      server.closeAllConnections();
      await new Promise((resolveClose, reject) => server.close((error) => error ? reject(error) : resolveClose()));
    }
  }
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual compiled component evidence under the shell CSP. No installed backend/persistence proof.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import { hostname, homedir } from "node:os";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import path from "node:path";

export const CODE_EDITOR_STATES = Object.freeze(["default", "empty", "hover", "focus", "edited", "undo", "redo", "readonly", "disabled", "required", "invalid", "error"]);
const modes = ["text", "javascript", "xml", "qweb", "scss", "python", "json", "bash"];
const markerProbeNames = ["ace_selection", "ace_active-line", "ace_selected-word", "ace_highlight-marker", "ace_selection+ace_highlight-marker", "ace_highlight-marker+ace_selection", "ace_active-line+ace_selection", "ace_selection+ace_active-line", "ace_selected-word+ace_selection", "ace_selection+ace_selected-word"];
const partialScope = "moli-native-selection-excluded";
const partialExcludedChecks = ["sourceGeometry", "caretGeometry", "nativeCursorRestoration", "nativeSelectionGeometry"];
const widths = [1280, 820, 390], themes = ["light", "dark"];
const interactions = ["input", "paste", "history", "sessions", "cursor", "readonly", "disabled", "keyboardExit", "parentEcho", "literalSource", "strictCsp", "externalStyles", "noExternalRequests", "disposal", "keyboardEntry", "keyboardReverseExit", "readonlySelectionCopy", "readonlyEditRoutes", "compositionAdmission", "pointerToolbar", "touchToolbar", "extensionShortcuts", "perFlushHistory", "stylesheetComposition", "tokenContrast"];
const sha = value => createHash("sha256").update(value).digest("hex");
const fail = message => { throw new Error(message); };

/** The sole automation exception is the immediate frame observed in the pinned Playwright
 * UtilityScript. A matching ancestor, source-file caller or changed signature remains refused. */
export function classifyPlaywrightEvalCaller(stack) {
  if (typeof stack !== "string" || stack.length > 16384) return null;
  const lines = stack.split("\n");
  if (lines[0] !== "Error: Code editor eval caller") return null;
  const guard = /^\s+at Object\.apply \(http:\/\/127\.0\.0\.1:([1-9][0-9]{0,4})\/guard\.js:[1-9][0-9]*:[1-9][0-9]*\)$/.exec(lines[1] ?? "");
  if (!guard || Number(guard[1]) > 65535) return null;
  const signature = "UtilityScript.evaluate (<anonymous>:290:30)";
  return lines[2]?.trim() === `at ${signature}` ? signature : null;
}

/** Default to the project route; native Chromium requires an explicit authorized selection. */
export function codeEditorBrowserRoute(value) {
  if (value === undefined) return "moli";
  if (value === "moli" || value === "chromium") return value;
  return fail("CODE_BROWSER_ROUTE must be moli or chromium.");
}

/** Explicit partial execution is restricted to Moli and never silently selected. */
export function codeEditorBrowserScope(value, route) {
  if (value === undefined || value === "full") return "full";
  if (value === partialScope && route === "moli") return partialScope;
  return fail("CODE_BROWSER_SCOPE must be full, or moli-native-selection-excluded with the moli route.");
}

/** Same-node DOM measurement used by both the actual browser and scoped DOM refusal tests.
 * Self-contained so Playwright can serialize this exact function without module closures. */
export function measureCodeEditorContrastCoverage(findings) {
  const rgba = value => { const match = /^rgba?\(([^)]+)\)$/.exec(value); if (!match) throw new Error(`Unsupported computed colour ${value}`); const parts = match[1].split(",").map(Number); return [...parts.slice(0, 3), parts.length === 4 ? parts[3] : 1]; };
  const blend = (front, back) => { const alpha = front[3] + back[3] * (1 - front[3]); if (alpha === 0) return [0, 0, 0, 0]; return [0, 1, 2].map(index => (front[index] * front[3] + back[index] * back[3] * (1 - front[3])) / alpha).concat(alpha); };
  const luminance = value => value.slice(0, 3).reduce((sum, channel, index) => { const linear = channel / 255; return sum + (linear <= .04045 ? linear / 12.92 : ((linear + .055) / 1.055) ** 2.4) * [.2126, .7152, .0722][index]; }, 0);
  const ratio = (foreground, background) => { const first = luminance(foreground), second = luminance(background); return (Math.max(first, second) + .05) / (Math.min(first, second) + .05); };
  const unsupportedPaint = node => {
    const style = getComputedStyle(node), effects = {};
    // Empty values are admitted only for optional CSS properties unavailable in that renderer;
    // an unavailable property cannot contribute a painted effect. Core properties require exact defaults.
    const admitted = {
      "background-image": ["none"], "background-blend-mode": ["normal"], "mix-blend-mode": ["normal"], "filter": ["none"],
      "backdrop-filter": ["none", ""], "-webkit-backdrop-filter": ["none", ""],
      "mask-image": ["none", ""], "-webkit-mask-image": ["none", ""], "text-shadow": ["none", ""],
    };
    for (const [property, defaults] of Object.entries(admitted)) {
      const value = style.getPropertyValue(property);
      if (!defaults.includes(value)) effects[property] = value;
    }
    return Object.keys(effects).length ? { node: { tag: node.tagName, id: node.id, className: node.className }, effects } : null;
  };
  return findings.map(finding => {
    if (finding.target.length !== 1 || typeof finding.target[0] !== "string") return { target: finding.target, inTextLayer: false, reason: "A nested selector is not admitted by this policy." };
    const matches = document.querySelectorAll(finding.target[0]);
    if (matches.length !== 1) return { target: finding.target, resolvedNodes: matches.length, inTextLayer: false, reason: "Selector does not identify exactly one node." };
    const node = matches[0], editor = node.closest(".code-editor"), layer = node.closest(".ace_text-layer"), inTextLayer = !!editor && !!layer && editor.contains(layer);
    if (!inTextLayer) return { target: finding.target, resolvedNodes: 1, inTextLayer: false, reason: "Node is outside the editor text layer." };
    const text = [...node.childNodes].filter(child => child.nodeType === Node.TEXT_NODE).map(child => child.textContent).join("");
    const ranges = [...node.childNodes].filter(child => child.nodeType === Node.TEXT_NODE && child.textContent.trim()).flatMap(child => { const range = document.createRange(); range.selectNodeContents(child); return [...range.getClientRects()].map(rect => ({ left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom })); });
    const colour = rgba(getComputedStyle(node).color), content = node.closest(".ace_content");
    const refused = (reason, witness) => ({ target: finding.target, resolvedNodes: 1, scope: ".code-editor .ace_text-layer", inTextLayer: true, text, reason, witness });
    if (!content || !editor.contains(content)) return refused("Text-layer node has no owned content plane.", { className: node.className });
    for (let current = node; current; current = current.parentElement) {
      const witness = unsupportedPaint(current);
      if (witness) return refused("Node or ancestor uses unsupported paint effects.", witness);
    }
    // A layer can paint an image/filter/background even when it has no marker children.
    // Establish the complete intersecting sibling plane before examining individual markers.
    for (const markerLayer of editor.querySelectorAll(".ace_marker-layer")) {
      const box = markerLayer.getBoundingClientRect(), style = getComputedStyle(markerLayer);
      const intersects = ranges.some(rect => box.left < rect.right && box.right > rect.left && box.top < rect.bottom && box.bottom > rect.top);
      if (style.display === "none" || style.visibility !== "visible" || !intersects) continue;
      if (!content.contains(markerLayer)) return refused("Marker layer or ancestor uses unsupported paint effects.", { node: { tag: markerLayer.tagName, className: markerLayer.className }, effects: { ownership: "Layer is outside the measured content plane." } });
      for (let current = markerLayer; current && current !== content; current = current.parentElement) {
        const currentStyle = getComputedStyle(current), witness = unsupportedPaint(current), background = rgba(currentStyle.backgroundColor);
        if (witness || Number(currentStyle.opacity) !== 1 || background[3] !== 0) return refused("Marker layer or ancestor uses unsupported paint effects.", witness ?? { node: { tag: current.tagName, className: current.className }, effects: { opacity: currentStyle.opacity, backgroundColor: currentStyle.backgroundColor } });
      }
    }
    const variants = [{ variant: "base", marker: null }], intersectingMarkers = [];
    for (const marker of editor.querySelectorAll(".ace_marker-layer > div")) {
      const box = marker.getBoundingClientRect(), style = getComputedStyle(marker);
      const intersects = ranges.some(rect => box.left < rect.right && box.right > rect.left && box.top < rect.bottom && box.bottom > rect.top);
      if (style.display === "none" || style.visibility !== "visible" || !intersects) continue;
      if (!content.contains(marker)) return refused("Marker layer or ancestor uses unsupported paint effects.", { node: { tag: marker.tagName, className: marker.className }, effects: { ownership: "Marker is outside the measured content plane." } });
      // An image/filter can paint even when background-color is fully transparent. Establish
      // geometry/visibility first, refuse unsupported paint, and only then inspect colour alpha.
      const witness = unsupportedPaint(marker);
      if (witness || Number(style.opacity) !== 1) return refused("Intersecting marker uses unsupported paint effects.", witness ?? { node: { tag: marker.tagName, className: marker.className }, effects: { opacity: style.opacity } });
      for (let current = marker.parentElement; current && current !== content; current = current.parentElement) {
        const currentStyle = getComputedStyle(current), ancestorWitness = unsupportedPaint(current), background = rgba(currentStyle.backgroundColor);
        if (ancestorWitness || Number(currentStyle.opacity) !== 1 || background[3] !== 0) return refused("Marker layer or ancestor uses unsupported paint effects.", ancestorWitness ?? { node: { tag: current.tagName, className: current.className }, effects: { opacity: currentStyle.opacity, backgroundColor: currentStyle.backgroundColor } });
      }
      const background = rgba(style.backgroundColor);
      if (background[3] > 0) intersectingMarkers.push({ className: marker.className, background, rect: { left: box.left, right: box.right, top: box.top, bottom: box.bottom } });
    }
    // Rectangle intersections do not establish exact pixel coverage or paint order. Evaluate
    // every ordered nonempty subset of at most four backgrounds (65 variants including base),
    // conservatively including cumulative active-line/selection/word backgrounds in both orders.
    if (intersectingMarkers.length > 4) return { target: finding.target, resolvedNodes: 1, scope: ".code-editor .ace_text-layer", inTextLayer: true, text, reason: "More than four marker backgrounds intersect this node; contrast composition is unsupported." };
    const combinations = (order, remaining) => {
      if (order.length) {
        const background = order.reduce((painted, index) => blend(intersectingMarkers[index].background, painted), [0, 0, 0, 0]);
        variants.push({ variant: "conservative-marker-composition", marker: { background, candidates: order.map(index => intersectingMarkers[index]) } });
      }
      for (const index of remaining) combinations([...order, index], remaining.filter(candidate => candidate !== index));
    };
    combinations([], intersectingMarkers.map((_marker, index) => index));
    const pixel = (foreground, marker) => {
      let painted = foreground ? [...colour] : [0, 0, 0, 0];
      for (let current = node; current; current = current.parentElement) {
        const style = getComputedStyle(current);
        if (marker && current === content) painted = blend(painted, marker.background);
        painted = blend(painted, rgba(style.backgroundColor));
        const opacity = Number(style.opacity); if (!Number.isFinite(opacity) || opacity < 0 || opacity > 1) throw new Error("Invalid computed ancestor opacity.");
        painted[3] *= opacity;
      }
      return blend(painted, [255, 255, 255, 1]);
    };
    const measured = variants.map(variant => { const foreground = pixel(true, variant.marker), background = pixel(false, variant.marker); return { ...variant, foreground, background, ratio: ratio(foreground, background) }; });
    const worst = measured.reduce((first, next) => next.ratio < first.ratio ? next : first);
    return { target: finding.target, resolvedNodes: 1, scope: ".code-editor .ace_text-layer", inTextLayer: true, text, className: node.className, foreground: worst.foreground, background: worst.background, ratio: worst.ratio, minimumRatio: 4.5, ranges, variants: measured };
  });
}

const incompletePolicyMessage = "20261103T155931Z-5c827a";
const incompletePolicyReasons = ["bgOverlap", "shortTextContent"];
const textLayerScope = ".code-editor .ace_text-layer";
const same = (first, second) => JSON.stringify(first) === JSON.stringify(second);
function measuredContrast(foreground, background) {
  const opaque = value => Array.isArray(value) && value.length === 4 && value[3] === 1 && value.slice(0, 3).every(channel => Number.isFinite(channel) && channel >= 0 && channel <= 255);
  if (!opaque(foreground) || !opaque(background)) return null;
  const luminance = colour => colour.slice(0, 3).reduce((sum, channel, index) => {
    const linear = channel / 255;
    return sum + (linear <= .04045 ? linear / 12.92 : ((linear + .055) / 1.055) ** 2.4) * [.2126, .7152, .0722][index];
  }, 0);
  const first = luminance(foreground), second = luminance(background);
  return (Math.max(first, second) + .05) / (Math.min(first, second) + .05);
}
function validNodeCoverage(measurement) {
  if (!measurement || measurement.reason !== undefined || measurement.witness !== undefined || measurement.inTextLayer !== true || measurement.scope !== textLayerScope || measurement.resolvedNodes !== 1 || typeof measurement.text !== "string" || !measurement.text.trim() || measurement.minimumRatio !== 4.5 || !Number.isFinite(measurement.ratio) || measurement.ratio < 4.5 || !Array.isArray(measurement.variants) || !measurement.variants.length) return false;
  for (const variant of measurement.variants) {
    const ratio = variant && measuredContrast(variant.foreground, variant.background);
    if (ratio === null || !Number.isFinite(variant.ratio) || variant.ratio < 4.5 || Math.abs(variant.ratio - ratio) > .000001) return false;
  }
  const worst = measurement.variants.reduce((first, next) => next.ratio < first.ratio ? next : first);
  return Math.abs(worst.ratio - measurement.ratio) <= .000001 && same(worst.foreground, measurement.foreground) && same(worst.background, measurement.background);
}
// Only the exact lead-authorized reasons on the exact measured capture node are eligible.
// Unexcused findings stay explicit and are refused by final receipt admission.
function contrastDisposition(entry) {
  const excused = [], unexcused = [];
  if (!Array.isArray(entry.contrastIncomplete)) return { excused, unexcused: [{ reason: "Missing raw incomplete findings." }] };
  for (const finding of entry.contrastIncomplete) {
    let reason;
    if (!finding || finding.id !== "color-contrast" || !Array.isArray(finding.target) || finding.target.length !== 1 || typeof finding.target[0] !== "string" || !finding.target[0]) reason = "Invalid incomplete target.";
    else if (!Array.isArray(finding.reasons) || !finding.reasons.length || new Set(finding.reasons).size !== finding.reasons.length || finding.reasons.some(value => !incompletePolicyReasons.includes(value)) || !Array.isArray(finding.checks) || !finding.checks.length || finding.checks.some(check => !check || check.id !== "color-contrast" || !incompletePolicyReasons.includes(check.messageKey) || check.data !== undefined && (!check.data || check.data.messageKey !== check.messageKey)) || !same([...new Set(finding.checks.map(check => check.messageKey))], finding.reasons)) reason = "Incomplete reason is outside the lead policy or does not match raw checks.";
    else {
      const measurements = Array.isArray(entry.contrastCoverage) ? entry.contrastCoverage.filter(measurement => measurement && same(measurement.target, finding.target)) : [];
      if (measurements.length !== 1 || !validNodeCoverage(measurements[0])) reason = "Exact text-layer node has no sufficient same-capture computed contrast.";
    }
    if (reason) unexcused.push({ target: finding?.target, reasons: finding?.reasons, reason });
    else excused.push({ target: finding.target, reasons: finding.reasons, policyMessage: incompletePolicyMessage });
  }
  return { excused, unexcused };
}

/** Independent receipt admission: the screenshot loop cannot shrink its own acceptance inventory. */
export function validateCodeEditorManifest(manifest) {
  if (manifest?.scope === partialScope || manifest?.acceptance === "partial" || manifest?.partialAcceptance === true) fail("Partial code editor evidence cannot establish full browser acceptance.");
  if (manifest?.scope !== undefined && manifest.scope !== "full") fail("Code editor evidence has an unknown execution scope.");
  validateCodeEditorReceipt(manifest, false);
}

/** Scoped admission retains every unrelated check and never establishes full acceptance. */
export function validateCodeEditorPartialManifest(manifest) {
  if (!manifest || manifest.route !== "moli" || manifest.scope !== partialScope || manifest.acceptance !== "partial" || manifest.completeAcceptance !== false || manifest.partialAcceptance !== true) fail("Scoped code editor evidence must be explicitly partial Moli evidence.");
  if (!same(manifest.excludedChecks, partialExcludedChecks) || !Array.isArray(manifest.exclusions) || !manifest.exclusions.length || manifest.exclusions.length > 1024) fail("Scoped code editor evidence requires its exact native-selection exclusion ledger.");
  for (const exclusion of manifest.exclusions) {
    if (!exclusion || !partialExcludedChecks.includes(exclusion.check) || !themes.includes(exclusion.theme) || !widths.includes(exclusion.width) || typeof exclusion.reason !== "string" || !exclusion.reason.trim() || exclusion.state !== undefined && !CODE_EDITOR_STATES.includes(exclusion.state)) fail("Scoped code editor evidence contains an unsupported exclusion.");
    if (!["nativeCursorRestoration", "nativeSelectionGeometry"].includes(exclusion.check) && (exclusion.observedFailure !== true || !exclusion.observed || typeof exclusion.observed !== "object")) fail("Scoped geometry exclusion requires its observed failing measurement.");
  }
  if (!manifest.exclusions.some(exclusion => exclusion.check === "sourceGeometry" || exclusion.check === "caretGeometry")) fail("Scoped code editor evidence did not reproduce the native-layout limitation.");
  if (manifest.checks?.cursor !== false) fail("Scoped code editor evidence must leave native cursor restoration unestablished.");
  validateCodeEditorReceipt(manifest, true);
}

/** Refuse stale cross-route evidence; callers retain both raw receipts and their hashes. */
export function validateCodeEditorRouteComparison(native, partial) {
  validateCodeEditorManifest(native);
  if (native.route !== "chromium" || native.completeAcceptance !== true) fail("Code editor route comparison requires complete native Chromium acceptance.");
  validateCodeEditorPartialManifest(partial);
  for (const key of ["revision", "policy", "fixture_entry_sha256", "javascript_sha256", "sourceFiles", "stylesheetComposition"]) {
    if (native[key] === undefined || !same(native[key], partial[key])) fail(`Code editor route comparison differs in ${key}.`);
  }
}

function validateCodeEditorReceipt(manifest, partial) {
  if (!manifest || manifest.schema !== 1 || !/^[a-f0-9]{40}$/.test(manifest.revision)) fail("Code editor evidence requires an exact Git revision.");
  if (manifest.route !== "moli" && manifest.route !== "chromium") fail("Code editor evidence requires an explicit browser route.");
  if (typeof manifest.browser !== "string" || !manifest.browser.trim()) fail("Code editor evidence requires the actual browser version.");
  if (!Array.isArray(manifest.sourceFiles) || !manifest.sourceFiles.length || manifest.sourceFiles.some(source => !source.path || !/^[a-f0-9]{64}$/.test(source.sha256))) fail("Code editor evidence requires source hashes.");
  if (!Array.isArray(manifest.responses)) fail("Code editor evidence requires HTTP response diagnostics.");
  for (const response of manifest.responses) {
    if (!response || typeof response.url !== "string" || !Number.isInteger(response.status)) fail("Code editor evidence has an invalid HTTP response diagnostic.");
    if (response.status >= 400) fail(`Code editor evidence has HTTP failure ${response.status} at ${response.url}.`);
  }
  if (!Array.isArray(manifest.requestFailures) || manifest.requestFailures.length) fail("Code editor evidence contains failed browser requests.");
  if (!Array.isArray(manifest.consoleErrors) || manifest.consoleErrors.length) fail("Code editor evidence contains browser errors.");
  if (!Array.isArray(manifest.cspViolations) || manifest.cspViolations.length) fail("Code editor evidence contains CSP violations.");
  if (!Array.isArray(manifest.forbiddenCalls) || manifest.forbiddenCalls.length) fail("Code editor evidence attempted dynamic code or worker construction.");
  if (!Array.isArray(manifest.evalBypasses)) fail("Code editor evidence requires an eval bypass ledger.");
  for (const [index, bypass] of manifest.evalBypasses.entries()) {
    if (!bypass || Object.keys(bypass).sort().join(",") !== "caller,operation,sequence,stack" || bypass.sequence !== index + 1 || bypass.operation !== "eval" || bypass.caller !== "UtilityScript.evaluate (<anonymous>:290:30)" || classifyPlaywrightEvalCaller(bypass.stack) !== bypass.caller) fail("Code editor evidence has an invalid eval bypass ledger entry.");
  }
  if (manifest.styleInjectionCount !== 0) fail("Code editor evidence attempted style-element injection.");
  if (!Array.isArray(manifest.failures) || manifest.failures.length) fail("Code editor evidence contains failed checks.");
  for (const check of interactions) if (!(partial && check === "cursor") && manifest.checks?.[check] !== true) fail(`Code editor evidence did not establish ${check}.`);
  for (const mode of modes) if (!manifest.checks?.modes?.includes(mode)) fail(`Code editor evidence did not establish mode ${mode}.`);
  if (manifest.contrastPresentationVersion !== undefined && manifest.contrastPresentationVersion !== 1) fail("Code editor evidence has an unsupported contrast presentation version.");
  const contrastFound = new Set();
  if (!Array.isArray(manifest.contrastSamples)) fail("Code editor evidence requires mode contrast samples.");
  for (const sample of manifest.contrastSamples) {
    if (!sample || !themes.includes(sample.theme) || !widths.includes(sample.width) || !modes.includes(sample.mode)) fail("Code editor evidence has an invalid mode contrast sample.");
    const key = `${sample.theme}/${sample.width}/${sample.mode}`;
    if (contrastFound.has(key)) fail(`Code editor evidence repeats contrast sample ${key}.`);
    contrastFound.add(key);
    if (!Array.isArray(sample.tokens) || !sample.tokens.length || sample.tokens.some(token => !token || typeof token.text !== "string" || !token.text.trim() || typeof token.className !== "string" || !Number.isFinite(token.ratio) || token.ratio < 4.5)) fail(`Code editor evidence has insufficient token contrast in ${key}.`);
    if (!Array.isArray(sample.markerTokens) || sample.markerTokens.some(token => !token || !markerProbeNames.includes(token.marker) || !Number.isFinite(token.ratio) || token.ratio < 4.5) || markerProbeNames.some(marker => !sample.markerTokens.some(token => token.marker === marker))) fail(`Code editor evidence has insufficient marker contrast in ${key}.`);
    if (manifest.contrastPresentationVersion === 1) for (const presentation of ["auto", "explicit"]) {
      if (sample.tokens.some(token => !["auto", "explicit"].includes(token.presentation)) || sample.markerTokens.some(token => !["auto", "explicit"].includes(token.presentation)) || !sample.tokens.some(token => token.presentation === presentation) || markerProbeNames.some(marker => !sample.markerTokens.some(token => token.marker === marker && token.presentation === presentation))) fail(`Code editor evidence is missing ${presentation} presentation contrast in ${key}.`);
    }
  }
  for (const theme of themes) for (const width of widths) for (const mode of modes) if (!contrastFound.has(`${theme}/${width}/${mode}`)) fail(`Code editor evidence is missing contrast sample ${theme}/${width}/${mode}.`);
  const found = new Set();
  if (!Array.isArray(manifest.entries)) fail("Code editor evidence requires captures.");
  for (const entry of manifest.entries) {
    const key = `${entry.theme}/${entry.width}/${entry.state}`;
    if (!themes.includes(entry.theme) || !widths.includes(entry.width) || !CODE_EDITOR_STATES.includes(entry.state)) fail(`Code editor evidence has an unknown capture ${key}.`);
    if (found.has(key)) fail(`Code editor evidence repeats ${key}.`);
    found.add(key);
    if (!/^[a-z0-9-]+\.png$/.test(entry.screenshot) || !/^[a-f0-9]{64}$/.test(entry.sha256) || !/^[a-z0-9-]+\.axe\.json$/.test(entry.axe)) fail(`Code editor evidence has invalid artifacts in ${key}.`);
    if (!Array.isArray(entry.violations) || entry.violations.length) fail(`Code editor evidence has accessibility violations in ${key}.`);
    const disposition = contrastDisposition(entry);
    if (disposition.unexcused.length || entry.contrastUnexcused !== undefined && (!Array.isArray(entry.contrastUnexcused) || entry.contrastUnexcused.length) || (disposition.excused.length ? !Array.isArray(entry.contrastExcused) || entry.contrastExcused.length !== disposition.excused.length || entry.contrastExcused.some((finding, index) => !finding || !same(finding.target, disposition.excused[index].target) || !same(finding.reasons, disposition.excused[index].reasons) || finding.policyMessage !== incompletePolicyMessage) : entry.contrastExcused !== undefined && (!Array.isArray(entry.contrastExcused) || entry.contrastExcused.length))) fail(`Code editor evidence has incomplete editor colour contrast in ${key}.`);
  }
  for (const theme of themes) for (const width of widths) for (const state of CODE_EDITOR_STATES) if (!found.has(`${theme}/${width}/${state}`)) fail(`Code editor evidence is missing ${theme}/${width}/${state}.`);
}

async function run() {
  const route = codeEditorBrowserRoute(process.env.CODE_BROWSER_ROUTE);
  const scope = codeEditorBrowserScope(process.env.CODE_BROWSER_SCOPE, route), scoped = scope === partialScope;
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const output = process.env.CODE_BROWSER_OUTPUT;
  assert.ok(output, "Set CODE_BROWSER_OUTPUT to a task-owned evidence directory.");
  await mkdir(output, { recursive: true, mode: 0o700 });
  const write = async (name, value) => { const target = path.join(output, name); await writeFile(target, value); await chmod(target, 0o600); };
  const require = createRequire(import.meta.url);
  const qa = path.join(homedir(), ".local/share/erpforall-qa/node_modules/playwright");
  const { chromium } = require(qa);
  const { build } = await import(pathToFileURL(path.join(root, "web/node_modules/esbuild/lib/main.js")).href);
  const client = relative => JSON.stringify(path.join(root, "addons/web/static/src/client", relative));
  const entry = `
import { createCodeEditor } from ${client("public.ts")};
const scene=document.querySelector('#scene');let editor, abort, events=[], errors=[], disposed=false, lastState;
const initial='const count = 1;\\nconst next = count + 1;';
function show(state='default',mode='javascript',value=initial){
 editor?.dispose();abort?.abort();scene.replaceChildren();events=[];errors=[];disposed=false;abort=new AbortController();
 const settings={label:'Automation source',value:state==='empty'||state==='required'?'':value,sessionId:'source-a',mode,signal:abort.signal,
 onChange:change=>events.push(change),report:error=>errors.push({kind:error?.kind,message:error?.message??String(error)})};
 if(state==='readonly')settings.readonly=true;
 if(state==='disabled')settings.disabled=true;
 if(state==='required')settings.required=true;
 if(state==='invalid')settings.invalid='Review this source before saving.';
 if(state==='error'){settings.value='ok';settings.limits={maxDocumentBytes:8};}
 editor=createCodeEditor(settings);scene.append(editor.node);return true;
}
function status(){if(disposed)return {...lastState,events,errors};return {value:editor.getValue(),sessionId:editor.getSessionId(),canUndo:editor.history.canUndo,canRedo:editor.history.canRedo,events,errors};}
function geometry(){
 const measure=node=>{if(!node)return null;const box=node.getBoundingClientRect(),css=getComputedStyle(node);return {tag:node.tagName,className:node.className,text:node.textContent,hidden:node.hidden,rect:{x:box.x,y:box.y,width:box.width,height:box.height,left:box.left,right:box.right},scrollLeft:node.scrollLeft,scrollWidth:node.scrollWidth,clientWidth:node.clientWidth,css:{display:css.display,visibility:css.visibility,fontFamily:css.fontFamily,fontSize:css.fontSize,lineHeight:css.lineHeight,left:css.left,width:css.width,overflow:css.overflow,transform:css.transform}};};
 const surface=editor.node.querySelector('.ace_editor'),native=surface?.env?.editor,renderer=native?.renderer;
 const firstLine=editor.node.querySelector('.ace_line'),firstToken=firstLine?.firstChild,lastToken=firstLine?.lastChild;
 const tokenRect=node=>{if(!node)return null;const range=document.createRange();range.selectNodeContents(node);const box=range.getBoundingClientRect();return {text:node.textContent,x:box.x,right:box.right,width:box.width};};
 let caretRanges={available:false,reason:'Native cursor/font-metrics mapping is unavailable.'};
 if(native&&renderer?.$fontMetrics){
  try{
   const cursor=native.getCursorPosition(),screen=native.session.documentToScreenPosition(cursor),metrics=renderer.$fontMetrics;
   const line=metrics.$findElementForScreenRow(screen.row),mapped=line?metrics.$findColumnPosition(line,screen.column):null;
   if(mapped){
    const rectangle=box=>({left:box.left,right:box.right,top:box.top,bottom:box.bottom,x:box.x,y:box.y,width:box.width,height:box.height});
    const readRange=range=>({bounding:rectangle(range.getBoundingClientRect()),clientRects:typeof range.getClientRects==='function'?[...range.getClientRects()].map(rectangle):null});
    const collapsed=document.createRange();collapsed.setStart(mapped.node,mapped.offset);collapsed.setEnd(mapped.node,mapped.offset);
    const length=mapped.node.nodeType===Node.TEXT_NODE?mapped.node.nodeValue.length:0;
    let oneCharacter=null;
    if(length){const start=mapped.offset<length?mapped.offset:Math.max(0,mapped.offset-1),end=Math.min(length,start+1);const range=document.createRange();range.setStart(mapped.node,start);range.setEnd(mapped.node,end);oneCharacter={start,end,text:mapped.node.nodeValue.slice(start,end),...readRange(range)};}
    const layer=renderer.$textLayer.element;
    caretRanges={available:true,cursor,screen,offset:mapped.offset,overflow:mapped.overflow,nodeType:mapped.node.nodeType,nodeText:mapped.node.nodeValue,collapsed:readRange(collapsed),oneCharacter,textLayerRect:rectangle(layer.getBoundingClientRect()),padding:renderer.$padding,characterWidth:renderer.characterWidth,expectedCursorLeftFromCollapsedRange:renderer.$padding+collapsed.getBoundingClientRect().left-layer.getBoundingClientRect().left+mapped.overflow*renderer.characterWidth};
   }else caretRanges={available:false,reason:'No rendered text node maps to the native cursor.',cursor,screen};
  }catch(failure){caretRanges={available:false,reason:'Caret Range diagnostic failed.',message:failure.message};}
 }
 return {source:status(),caretRanges,surface:measure(surface),scroller:measure(editor.node.querySelector('.ace_scroller')),content:measure(editor.node.querySelector('.ace_content')),textLayer:measure(editor.node.querySelector('.ace_text-layer')),cursor:measure(editor.node.querySelector('.ace_cursor')),input:measure(editor.node.querySelector('textarea')),gutter:measure(editor.node.querySelector('.ace_gutter')),error:measure(editor.node.querySelector('.code-editor-error')),firstToken:tokenRect(firstToken),lastToken:tokenRect(lastToken),native:renderer?{available:true,cursor:native.getCursorPosition(),sessionScrollLeft:native.session.getScrollLeft(),rendererScrollLeft:renderer.scrollLeft,rendererScrollTop:renderer.scrollTop,characterWidth:renderer.characterWidth,lineHeight:renderer.lineHeight,scrollerWidth:renderer.scrollerWidth,scrollerHeight:renderer.scrollerHeight,size:renderer.$size,fontMetrics:renderer.$textLayer?.$fontMetrics?.$characterSize}: {available:false}};
}
window.fixture={show,initial,status,geometry,
 echo:()=>editor.setValue(editor.getValue()),setValue:value=>editor.setValue(value),session:options=>editor.setSession(options),focus:()=>editor.focus(),
 readonly:value=>editor.setReadonly(value),disabled:value=>editor.setDisabled(value),presentation:options=>editor.setPresentation(options),
 undo:()=>editor.history.undo(),redo:()=>editor.history.redo(),dispose:()=>{lastState=status();disposed=true;editor.dispose();},abort:()=>abort.abort()};
show();document.body.dataset.ready='true';
`;
  const bundle = await build({ stdin: { contents: entry, resolveDir: root, sourcefile: "code-editor-browser-fixture.ts", loader: "ts" }, bundle: true, write: false, format: "esm", platform: "browser", target: "es2023", metafile: true, alias: { "@erpforall/elang": path.join(root, "web/src/index.ts"), "@erpforall/expr": path.join(root, "web/packages/expr/src/index.ts") } });
  const javascript = bundle.outputFiles[0].contents;
  const clientManifest = JSON.parse(await readFile(path.join(root, "addons/web/static/client.json"), "utf8"));
  assert.ok(Array.isArray(clientManifest.styles) && clientManifest.styles.length > 0, "Client manifest styles are required.");
  const stylesheetInputs = [];
  for (const relative of clientManifest.styles) {
    assert.ok(typeof relative === "string" && relative.startsWith("src/") && relative.endsWith(".css") && !relative.split("/").includes(".."), "Invalid client stylesheet path.");
    const filename = `addons/web/static/${relative}`, contents = await readFile(path.join(root, filename));
    stylesheetInputs.push({ path: filename, contents, sha256: sha(contents) });
  }
  // Same ordered newline composition as build-human-ui.mjs. Fixture styles follow the shipped client bytes.
  const clientStylesheet = Buffer.concat(stylesheetInputs.flatMap(input => [input.contents, Buffer.from("\n")]));
  const assets = new Map([
    ["/fixture.js", [javascript, "text/javascript; charset=utf-8"]],
    ["/client.css", [clientStylesheet, "text/css; charset=utf-8"]],
    ["/axe.js", [await readFile(path.join(root, "web/node_modules/axe-core/axe.min.js")), "text/javascript; charset=utf-8"]],
    ["/fixture.css", [Buffer.from('body{margin:0;background:var(--canvas);color:var(--text)}main{box-sizing:border-box;padding:24px;max-width:900px;margin:auto}h1{font-size:22px}#scene{min-width:0;min-height:360px}#before,#after{margin-block:12px}@media(max-width:700px){main{padding:12px}}'), "text/css; charset=utf-8"]],
    ["/guard.js", [Buffer.from(`
window.codeCspViolations=[];window.codeForbiddenCalls=[];window.codeEvalBypasses=[];window.codeStyleInjectionCount=0;
const classifyPlaywrightEvalCaller=${classifyPlaywrightEvalCaller.toString()};
document.addEventListener("securitypolicyviolation",event=>window.codeCspViolations.push({directive:event.effectiveDirective,blockedURI:event.blockedURI}));
for(const key of ["eval","Function","Worker"]){
 const original=window[key];
 if(original)window[key]=new Proxy(original,{
  apply(_target,receiver,args){
   if(key==="eval"){
    const stack=new Error("Code editor eval caller").stack;
    const caller=classifyPlaywrightEvalCaller(stack);
    if(caller!==null){window.codeEvalBypasses.push({sequence:window.codeEvalBypasses.length+1,operation:"eval",caller,stack});return Reflect.apply(original,receiver,args);}
   }
   window.codeForbiddenCalls.push(key);throw new Error("Forbidden dynamic browser operation: "+key);
  },
  construct(){window.codeForbiddenCalls.push(key);throw new Error("Forbidden dynamic browser operation: "+key);}
 });
}
new MutationObserver(records=>{for(const record of records)for(const node of record.addedNodes)if(node.nodeType===1){if(node.tagName==="STYLE")window.codeStyleInjectionCount++;window.codeStyleInjectionCount+=node.querySelectorAll("style").length;}}).observe(document,{childList:true,subtree:true});
`), "text/javascript; charset=utf-8"]],
  ]);
  const policy = "default-src 'self'; script-src 'self'; style-src-elem 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'none'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";
  const html = '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Code editor verification</title><link rel="stylesheet" href="/client.css"><link rel="stylesheet" href="/fixture.css"><script src="/guard.js"></script><script src="/axe.js"></script></head><body><main><h1>Code editor verification</h1><p>Isolated compiled component; the owning screen saves source.</p><button type="button" id="before">Before editor</button><div id="scene"></div><button type="button" id="after">After editor</button></main><script type="module" src="/fixture.js"></script></body></html>';
  const serverRequests = [];
  const server = createServer((request, response) => {
    response.on("finish", () => serverRequests.push({ method: request.method, path: request.url, status: response.statusCode }));
    const asset = assets.get(request.url);
    if (request.url === "/favicon.ico") { response.writeHead(204); response.end(); return; }
    if (asset) { response.writeHead(200, { "Content-Type": asset[1], "Cache-Control": "no-store" }); response.end(asset[0]); }
    else if (request.url === "/") { response.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Content-Security-Policy": policy, "Cache-Control": "no-store" }); response.end(html); }
    else { response.writeHead(404, { "Content-Type": "text/plain" }); response.end("Unknown code editor fixture route."); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  let moli, endpoint = null, moliVersion = null, moliOutput = "";
  if (route === "moli") {
    const portServer = createServer(); await new Promise(resolve => portServer.listen(0, "127.0.0.1", resolve));
    const cdpPort = portServer.address().port; await new Promise(resolve => portServer.close(resolve));
    const moliBin = process.env.MOLI_BIN ?? path.join(homedir(), ".local/bin/moli");
    const version = spawnSync(moliBin, ["--version"], { encoding: "utf8" }); assert.equal(version.status, 0, version.stderr || "Moli unavailable");
    moliVersion = version.stdout.trim();
    moli = spawn(moliBin, ["serve", "--host", "127.0.0.1", "--port", String(cdpPort), "--layout"], { stdio: ["ignore", "pipe", "pipe"] });
    for (const stream of [moli.stdout, moli.stderr]) stream.on("data", chunk => { moliOutput += chunk; if (moliOutput.length > 1048576) { moli.kill("SIGTERM"); fail("Moli diagnostics exceeded the fixture bound."); } });
    endpoint = `http://127.0.0.1:${cdpPort}`;
  }
  const sourceFiles = [];
  for (const filename of Object.keys(bundle.metafile.inputs).sort()) {
    if (filename === "code-editor-browser-fixture.ts") continue;
    const absolute = path.resolve(root, filename);
    sourceFiles.push({ path: path.relative(root, absolute), sha256: sha(await readFile(absolute)) });
  }
  for (const filename of ["web/scripts/browser-editors-code.mjs", "web/pnpm-lock.yaml", "addons/web/static/client.json", ...stylesheetInputs.map(input => input.path)]) sourceFiles.push({ path: filename, sha256: sha(await readFile(path.join(root, filename))) });
  let revision;
  try { revision = (await readFile(path.join(root, "REVISION"), "utf8")).trim(); }
  catch (error) { if (error.code !== "ENOENT") throw error; const git = spawnSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }); assert.equal(git.status, 0, git.stderr); revision = git.stdout.trim(); }
  assert.match(revision, /^[a-f0-9]{40}$/);
  const manifest = { schema: 1, generated_at: new Date().toISOString(), host: hostname(), node: process.version, route, scope, contrastPresentationVersion: 1, ...(scoped ? { acceptance: "partial", completeAcceptance: false, partialAcceptance: false, excludedChecks: [...partialExcludedChecks], exclusions: [] } : {}), moli: moliVersion, playwright: require(path.join(qa, "package.json")).version, revision, policy, origin, cdp_endpoint: endpoint, fixture_entry_sha256: sha(entry), javascript_sha256: sha(javascript), sourceFiles, entries: [], contrastSamples: [], stylesheetComposition: { manifest: "addons/web/static/client.json", inputs: stylesheetInputs.map(({ path, sha256 }) => ({ path, sha256 })), sha256: sha(clientStylesheet) }, interactionDetails: [], checks: { modes: [] }, serverRequests, responses: [], requestFailures: [], consoleMessages: [], consoleErrors: [], cspViolations: [], forbiddenCalls: [], evalBypasses: [], styleInjectionCount: 0, failures: [], boundary: "Compiled-source component only; Synthetic ClipboardEvent and composing InputEvent exercise actual browser handlers and byte admission; OS clipboard permissions and native IME operation remain unverified. No installed backend, source execution, persistence, screen-reader speech, deployment or whole-view parity acceptance." };
  let browser, guardCdp;
  const captureGuardState = async () => {
    assert.ok(guardCdp, "Guard diagnostics require an attached CDP session.");
    // Raw CDP avoids executing another UtilityScript eval just to inspect its own bypass ledger.
    const response = await guardCdp.send("Runtime.evaluate", { expression: "JSON.stringify({cspViolations:window.codeCspViolations,forbiddenCalls:window.codeForbiddenCalls,evalBypasses:window.codeEvalBypasses,styleInjectionCount:window.codeStyleInjectionCount})", returnByValue: true });
    if (response.exceptionDetails || typeof response.result?.value !== "string") fail("Raw CDP could not read code editor guard diagnostics.");
    const value = JSON.parse(response.result.value);
    assert.ok(Array.isArray(value.cspViolations) && Array.isArray(value.forbiddenCalls) && Array.isArray(value.evalBypasses) && Number.isSafeInteger(value.styleInjectionCount), "Code editor guard diagnostics are incomplete.");
    manifest.cspViolations = value.cspViolations; manifest.forbiddenCalls = value.forbiddenCalls;
    manifest.evalBypasses = value.evalBypasses; manifest.styleInjectionCount = value.styleInjectionCount;
  };
  try {
    if (route === "moli") {
      let discovered = false;
      for (let attempt = 0; attempt < 100; attempt++) {
        if (moli.exitCode !== null) fail(`Moli exited before discovery: ${moliOutput}`);
        try { const response = await fetch(`${endpoint}/json/version`); if (response.ok) { discovered = true; break; } }
        catch (error) { if (!(error instanceof TypeError)) throw error; }
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      assert.ok(discovered, "Moli discovery did not become ready.");
      browser = await chromium.connectOverCDP(endpoint);
    } else browser = await chromium.launch({ headless: true });
    manifest.browser = browser.version();
    const context = route === "moli" ? browser.contexts()[0] : await browser.newContext();
    assert.ok(context, "The selected browser did not expose a context.");
    const page = await context.newPage(); guardCdp = await context.newCDPSession(page); page.setDefaultTimeout(8000); page.setDefaultNavigationTimeout(15000);
    const unexpectedRequests = [];
    page.on("pageerror", error => manifest.consoleErrors.push(error.message));
    page.on("console", message => { if (message.type() === "error") { manifest.consoleErrors.push(message.text()); manifest.consoleMessages.push({ type: message.type(), text: message.text(), location: message.location() }); } });
    page.on("response", response => manifest.responses.push({ url: response.url(), status: response.status(), contentType: response.headers()["content-type"] ?? null }));
    page.on("requestfailed", request => manifest.requestFailures.push({ url: request.url(), error: request.failure()?.errorText ?? "Request failed without an error message." }));
    page.on("request", request => { if (!request.url().startsWith(`${origin}/`)) unexpectedRequests.push(request.url()); });
    const status = () => page.evaluate(() => window.fixture.status());
    const waitForStatus = async (predicate, message) => {
      let observed;
      for (let attempt = 0; attempt < 100; attempt++) {
        observed = await status(); if (predicate(observed)) return observed;
        await new Promise(resolve => setTimeout(resolve, 25));
      }
      fail(`${message}: ${JSON.stringify(observed)}`);
    };
    const render = async () => { await page.screenshot({ fullPage: true }); };
    const renderChangedFeedback = async name => {
      const before = await page.evaluate(() => window.fixture.geometry());
      await render();
      const after = await page.evaluate(() => window.fixture.geometry());
      await write(`${name}.render-sync.json`, JSON.stringify({ before, after }, null, 2) + "\n");
      return after;
    };
    let showSequence = 0;
    const show = async (state = "default", mode, value) => {
      const sequence = ++showSequence;
      await page.evaluate(({ state, mode, value }) => window.fixture.show(state, mode, value), { state, mode, value });
      await render();
      const deadline = performance.now() + 8000;
      let observed;
      // Pure measurements only: the component must finish its own mount/render lifecycle.
      // Poll through the existing exact-caller page.evaluate route; injected predicate
      // helpers must not broaden the fixture's dynamic-code exception.
      for (;;) {
        observed = await page.evaluate(() => window.fixture.geometry());
        const ready = observed.scroller?.rect.width > 0 && observed.native?.characterWidth > 0 && observed.native?.lineHeight > 0
          && (observed.source.value.length === 0 || typeof observed.textLayer?.text === "string" && observed.textLayer.text.length > 0);
        if (ready) { await render(); return; }
        if (performance.now() >= deadline) break;
        await new Promise(resolve => setTimeout(resolve, Math.min(100, Math.max(0, deadline - performance.now()))));
      }
      await write(`show-${sequence}.mount-readiness.json`, JSON.stringify({
        state, mode: mode ?? "javascript", timeoutMilliseconds: 8000, observed,
        dom: await page.locator("#scene").innerHTML(),
        boundary: "Readiness timed out without changing the editor. Zero metrics or empty rendered text remain a failed check; cause is unverified.",
      }, null, 2) + "\n");
      fail(`Code editor mount/render readiness timed out after 8000ms for ${state}/${mode ?? "javascript"}; see show-${sequence}.mount-readiness.json.`);
    };
    const focus = async () => { await page.evaluate(() => window.fixture.focus()); };
    const type = async value => { await focus(); await page.keyboard.type(value); };
    const sourceInput = () => page.locator(".ace_text-input");
    const initial = await page.goto(origin, { waitUntil: "networkidle" }); assert.equal(initial.headers()["content-security-policy"], policy);
    await page.waitForSelector('body[data-ready="true"]');
    const modifier = await page.evaluate(() => /Mac|PPC/.test(navigator.platform) ? "Meta" : "Control");
    // Collect every mode/theme/viewport before enforcing contrast. A first axe failure must not hide
    // the dark-token baseline, and a failed run retains the complete computed-colour inventory.
    const contrastFailures = [];
    for (const theme of themes) for (const width of widths) {
      await page.setViewportSize({ width, height: 900 }); await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      const samples = { text: "plain source", javascript: "/** @param {number} count */\nconst count = /ready/;\nconsole.log(count + 42);", xml: '<node name="value"/>', qweb: '<t t-if="ready"><span t-out="name"/></t>', scss: "$accent: #123456; .item { color: $accent; }", python: "def answer():\n    print(42)\n    return True", json: '{"answer":42}', bash: '#!/bin/bash\necho "ready"' };
      for (const mode of modes) {
        await show("default", mode, samples[mode]); assert.equal(await page.locator("[data-code-mode]").getAttribute("data-code-mode"), mode); assert.equal((await status()).value, samples[mode]);
        if (mode !== "text") {
          const syntaxTokens = page.locator(".ace_line span[class*='ace_']");
          try {
            // Mode tokenization renders asynchronously. Wait for its owned token DOM instead
            // of interpreting the first count after a screenshot as final mode readiness.
            await syntaxTokens.first().waitFor({ state: "attached", timeout: 8000 });
            await render();
          } catch (error) {
            await write(`${theme}-${width}-${mode}.syntax-readiness.json`, JSON.stringify({
              theme, width, mode, message: error.message, observed: await status(),
              geometry: await page.evaluate(() => window.fixture.geometry()),
              textLayer: await page.locator(".ace_text-layer").innerHTML(),
              tokenCount: await syntaxTokens.count(),
            }, null, 2) + "\n");
            throw error;
          }
          assert.ok(await syntaxTokens.count() > 0, `${mode} has no rendered syntax tokens`);
        }
        const measured = { tokens: [], markerTokens: [] };
        for (const presentation of ["auto", "explicit"]) {
          await page.evaluate(({ presentation, theme }) => window.fixture.presentation({ theme: presentation === "auto" ? "auto" : theme }), { presentation, theme }); await render();
          const current = await page.evaluate(() => {
          const root = document.querySelector(".code-editor"), surface = root.querySelector(".ace_editor");
          const rgba = value => { const match = /^rgba?\(([^)]+)\)$/.exec(value); if (!match) throw new Error(`Unsupported computed colour ${value}`); const parts = match[1].split(",").map(Number); return [...parts.slice(0, 3), parts.length === 4 ? parts[3] : 1]; };
          const blend = (front, back) => { const alpha = front[3] + back[3] * (1 - front[3]); return [0, 1, 2].map(index => (front[index] * front[3] + back[index] * back[3] * (1 - front[3])) / alpha).concat(alpha); };
          const background = node => { const ancestors = []; for (let current = node; current; current = current.parentElement) ancestors.unshift(current); return ancestors.reduce((colour, current) => blend(rgba(getComputedStyle(current).backgroundColor), colour), [255, 255, 255, 1]); };
          const luminance = value => value.slice(0, 3).map(channel => { const linear = channel / 255; return linear <= .04045 ? linear / 12.92 : ((linear + .055) / 1.055) ** 2.4; }).reduce((total, linear, index) => total + linear * [.2126, .7152, .0722][index], 0);
          const ratio = (foreground, background) => { const first = luminance(blend(foreground, background)), second = luminance(background); return (Math.max(first, second) + .05) / (Math.min(first, second) + .05); };
          const tokens = [], textNodes = document.createTreeWalker(root.querySelector(".ace_text-layer"), NodeFilter.SHOW_TEXT);
          while (textNodes.nextNode()) { const text = textNodes.currentNode.textContent; if (!text.trim()) continue; const node = textNodes.currentNode.parentElement, style = getComputedStyle(node), back = background(node), foreground = rgba(style.color); tokens.push({ text, className: node.className, foreground: style.color, background: back, ratio: ratio(foreground, back) }); }
          // Markers are composited bottom-to-top. Measure active-line and selected-word
          // selection overlaps in both orders without assuming a native paint order. The
          // separate native-selection captures retain actual marker placement and text.
          const markerTokens = [], markerColours = new Map(), layer = root.querySelector(".ace_marker-layer");
          const record = (marker, markerBackground) => { for (const token of tokens) markerTokens.push({ marker, text: token.text, className: token.className, foreground: token.foreground, background: markerBackground, ratio: ratio(rgba(token.foreground), markerBackground) }); };
          for (const marker of ["ace_selection", "ace_active-line", "ace_selected-word", "ace_highlight-marker"]) {
            const probe = document.createElement("div"); probe.className = marker; layer.append(probe);
            const colour = rgba(getComputedStyle(probe).backgroundColor); markerColours.set(marker, colour); probe.remove();
            record(marker, blend(colour, background(surface)));
          }
          for (const order of [["ace_selection", "ace_highlight-marker"], ["ace_highlight-marker", "ace_selection"], ["ace_active-line", "ace_selection"], ["ace_selection", "ace_active-line"], ["ace_selected-word", "ace_selection"], ["ace_selection", "ace_selected-word"]]) {
            const markerBackground = order.reduce((painted, marker) => blend(markerColours.get(marker), painted), background(surface));
            record(order.join("+"), markerBackground);
          }
          return { tokens, markerTokens };
        });
          measured.tokens.push(...current.tokens.map(token => ({ ...token, presentation })));
          measured.markerTokens.push(...current.markerTokens.map(token => ({ ...token, presentation })));
        }
        const sample = { theme, width, mode, compositionOrder: "Marker names are ordered bottom-to-top; both orders are measured for overlapping backgrounds.", ...measured }; manifest.contrastSamples.push(sample);
        await write(`${theme}-${width}-${mode}.contrast.json`, JSON.stringify(sample, null, 2) + "\n");
        if (!measured.tokens.length) contrastFailures.push(`No contrast tokens for ${theme}/${width}/${mode}`);
        for (const token of measured.tokens) if (!Number.isFinite(token.ratio) || token.ratio < 4.5) contrastFailures.push(`Insufficient ${theme}/${width}/${mode} token contrast ${token.className} ${JSON.stringify(token.text)}: ${token.ratio}`);
        for (const token of measured.markerTokens) if (!Number.isFinite(token.ratio) || token.ratio < 4.5) contrastFailures.push(`Insufficient ${theme}/${width}/${mode} ${token.marker} contrast ${token.className}: ${token.ratio}`);
      }
    }
    await write("contrast-findings.json", JSON.stringify(contrastFailures, null, 2) + "\n");
    assert.deepEqual(contrastFailures, [], "Rendered code token/marker contrast findings (see contrast-findings.json)");
    for (const theme of themes) for (const width of widths) {
      await page.setViewportSize({ width, height: 900 }); await page.evaluate(value => { document.documentElement.dataset.theme = value; }, theme);
      for (const state of CODE_EDITOR_STATES) {
        const name = `${theme}-${width}-${state}`;
        try {
          await show(state);
          if (state === "hover") await page.locator(".code-editor").hover();
          if (state === "focus") await focus();
          if (["edited", "undo", "redo"].includes(state)) {
            const beforeEdit = (await status()).value;
            await focus(); await page.keyboard.press(`${modifier}+End`); await page.keyboard.type("!");
            const afterEdit = (await status()).value;
            assert.equal((await status()).canUndo, true); assert.notEqual(afterEdit, beforeEdit);
            if (state === "undo" || state === "redo") { await page.getByRole("button", { name: "Undo", exact: true }).click(); assert.equal((await status()).value, beforeEdit, "Pointer Undo did not restore the previous source"); }
            if (state === "redo") { await page.getByRole("button", { name: "Redo", exact: true }).click(); assert.equal((await status()).value, afterEdit, "Pointer Redo did not restore the edited source"); manifest.checks.pointerToolbar = true; manifest.interactionDetails.push({ theme, width, pointerToolbar: { beforeEdit, afterEdit, undoRestored: beforeEdit, redoRestored: afterEdit } }); }
          }
          if (state === "readonly") {
            const before = await status(); await type("blocked");
            await focus(); await page.keyboard.press(`${modifier}+a`);
            const readonlyRoutes = await sourceInput().evaluate(node => {
              const copyData = new DataTransfer(), copy = new ClipboardEvent("copy", { clipboardData: copyData, bubbles: true, cancelable: true }); node.dispatchEvent(copy);
              const pasteData = new DataTransfer(); pasteData.setData("text/plain", "blocked paste");
              const paste = new ClipboardEvent("paste", { clipboardData: pasteData, bubbles: true, cancelable: true }); node.dispatchEvent(paste);
              const cut = new ClipboardEvent("cut", { clipboardData: new DataTransfer(), bubbles: true, cancelable: true }); node.dispatchEvent(cut);
              const dropData = new DataTransfer(); dropData.setData("text/plain", "blocked drop");
              const drop = new DragEvent("drop", { dataTransfer: dropData, bubbles: true, cancelable: true }); node.dispatchEvent(drop);
              const history = ["undo", "redo"].map(operation => { try { window.fixture[operation](); return { operation, admitted: true }; } catch (error) { return { operation, kind: error.kind, message: error.message }; } });
              return { copied: copyData.getData("text/plain"), pastePrevented: paste.defaultPrevented, dropPrevented: drop.defaultPrevented, history };
            });
            assert.equal(readonlyRoutes.copied, before.value, "Readonly keyboard selection is copyable through the browser handler");
            assert.equal(readonlyRoutes.pastePrevented, true); assert.equal(readonlyRoutes.dropPrevented, true);
            assert.deepEqual(readonlyRoutes.history, [{ operation: "undo", kind: "validation", message: "Code editor is readonly." }, { operation: "redo", kind: "validation", message: "Code editor is readonly." }]);
            await page.keyboard.press(`${modifier}+z`);
            assert.equal((await status()).value, before.value); assert.equal((await status()).events.length, 0);
            manifest.interactionDetails.push({ theme, width, readonly: readonlyRoutes, retainedValue: (await status()).value });
            assert.equal(await page.locator(".ace_cursor").first().evaluate(node => getComputedStyle(node).display === "none" || getComputedStyle(node.parentElement).display === "none" || getComputedStyle(node).visibility === "hidden"), true);
          }
          if (state === "disabled") {
            assert.equal(await sourceInput().evaluate(node => node.disabled || node.tabIndex < 0), true);
            await page.locator("#before").focus(); for (let i = 0; i < 4; i++) { await page.keyboard.press("Tab"); assert.equal(await sourceInput().evaluate(node => node === document.activeElement), false); }
          }
          if (state === "required") assert.equal(await sourceInput().getAttribute("aria-required"), "true");
          if (state === "invalid") { assert.equal(await sourceInput().getAttribute("aria-invalid"), "true"); await renderChangedFeedback(name); await page.getByText("Review this source before saving.", { exact: true }).waitFor(); }
          if (state === "error") { await focus(); await page.keyboard.press(`${modifier}+End`); await page.keyboard.type(" document exceeds bound"); assert.ok((await status()).value.length <= 8); await renderChangedFeedback(name); await page.locator(".code-editor-error:not([hidden])").waitFor(); }
          assert.deepEqual((await status()).errors, [], `Owner reporter received a component failure in ${name}`);
          await render();
          const engineGeometry = await page.evaluate(() => window.fixture.geometry());
          await write(`${name}.engine-geometry.json`, JSON.stringify(engineGeometry, null, 2) + "\n");
          // DOM text equality cannot establish that the user can see the source or caret.
          // Short fixture lines fit the viewport, so a visible gutter must not obscure a token.
          const scroller = engineGeometry.scroller;
          assert.ok(scroller && scroller.rect.width > 0, `Rendered code scroller has no width in ${name}`);
          const gutter = engineGeometry.gutter;
          const readableLeft = gutter && gutter.css.display !== "none" && gutter.rect.width > 0 ? Math.max(scroller.rect.left, gutter.rect.right) : scroller.rect.left;
          if (engineGeometry.source.value) assert.ok(engineGeometry.firstToken?.text && engineGeometry.lastToken?.text, `Nonempty code has no token measurement in ${name}`);
          const geometryCheck = (check, assertion) => {
            try { assertion(); }
            catch (error) {
              const expected = check === "sourceGeometry" ? `Rendered code source is clipped or obscured in ${name}; see engine-geometry receipt` : `Rendered code caret disagrees with its source Range in ${name}; see engine-geometry receipt`;
              // Only the reproduced layout assertion is excluded. Missing layers, unavailable
              // measurements, TypeError and new failure modes still stop the scoped run.
              if (!scoped || error.code !== "ERR_ASSERTION" || !error.message.startsWith(expected)) throw error;
              manifest.exclusions.push({ check, theme, width, state, observedFailure: true, reason: error.message, observed: engineGeometry });
            }
          };
          geometryCheck("sourceGeometry", () => {
            for (const token of [engineGeometry.firstToken, engineGeometry.lastToken]) if (token?.text) assert.ok(token.width > 0 && token.x >= readableLeft - 1 && token.right <= scroller.rect.right + 1, `Rendered code source is clipped or obscured in ${name}; see engine-geometry receipt`);
          });
          if (["focus", "edited", "undo", "redo", "error"].includes(state) && engineGeometry.source.value) {
            const probe = engineGeometry.caretRanges;
            geometryCheck("caretGeometry", () => {
            assert.equal(probe.available, true, `Caret Range measurement is unavailable in ${name}`);
            // Physical rectangles include Ace's supported HiDPI transform positioning. CSS left
            // alone is not the painted caret position when translateX supplies its offset.
            const actualLeft = engineGeometry.cursor?.rect.left;
            const expectedLeft = probe.collapsed.bounding.left + probe.overflow * probe.characterWidth;
            assert.ok(Number.isFinite(actualLeft) && Number.isFinite(expectedLeft) && Math.abs(actualLeft - expectedLeft) <= 1, `Rendered code caret disagrees with its source Range in ${name}; see engine-geometry receipt`);
            });
          }
          const geometry = await page.evaluate(() => {
            const editor = document.querySelector(".code-editor"), box = editor.getBoundingClientRect();
            return { width: innerWidth, pageWidth: document.documentElement.scrollWidth, editor: { x: box.x, right: box.right, width: box.width, height: box.height }, buttons: [...editor.querySelectorAll("button")].map(node => { const rect = node.getBoundingClientRect(); return { name: node.textContent, width: rect.width, height: rect.height }; }), inlineStyles: document.querySelectorAll("style").length, stylesheets: [...document.styleSheets].map(sheet => sheet.href) };
          });
          assert.ok(geometry.editor.width > 0 && geometry.editor.height > 0); assert.ok(geometry.pageWidth <= width + 1, `Page overflow ${name}`);
          assert.ok(geometry.editor.x >= -1 && geometry.editor.right <= width + 1, `Editor overflow ${name}`);
          for (const button of geometry.buttons) assert.ok(button.width >= 24 && button.height >= 24, `Undersized ${name} ${button.name}`);
          assert.equal(geometry.inlineStyles, 0, "Engine injected style elements under strict CSP");
          for (const file of ["client.css"]) assert.ok(geometry.stylesheets.some(href => href?.endsWith(`/${file}`)), `Missing external ${file}`);
          const axe = await page.evaluate(async () => window.axe.run(document));
          const contrastIncomplete = await page.evaluate(results => results.filter(result => result.id === "color-contrast").flatMap(result => result.nodes.filter(node => node.target.some(selector => typeof selector === "string" && document.querySelector(selector)?.closest(".code-editor"))).map(node => {
            const checks = [...node.any, ...node.all, ...node.none].map(check => ({ id: check.id, messageKey: check.data?.messageKey ?? null, data: check.data, message: check.message }));
            return { id: result.id, target: node.target, reasons: [...new Set(checks.map(check => check.messageKey))], checks, failureSummary: node.failureSummary };
          })), axe.incomplete);
          const contrastCoverage = await page.evaluate(measureCodeEditorContrastCoverage, contrastIncomplete);
          await write(`${name}.axe.json`, JSON.stringify(axe, null, 2) + "\n"); assert.deepEqual(axe.violations, [], `Accessibility findings ${name}`);
          // Retain every raw incomplete result. Final admission permits only the lead-authorized
          // text-layer reasons with sufficient computed coverage for that same node/capture.
          await page.screenshot({ path: path.join(output, `${name}.png`), fullPage: true }); await chmod(path.join(output, `${name}.png`), 0o600);
          const capture = { theme, width, state, screenshot: `${name}.png`, sha256: sha(await readFile(path.join(output, `${name}.png`))), axe: `${name}.axe.json`, violations: axe.violations, contrastIncomplete, contrastCoverage, geometry, observed: await status(), engineGeometry: `${name}.engine-geometry.json` };
          const disposition = contrastDisposition(capture);
          capture.contrastExcused = disposition.excused; capture.contrastUnexcused = disposition.unexcused;
          await write(`${name}.contrast-exceptions.json`, JSON.stringify({ policyMessage: incompletePolicyMessage, contrastIncomplete, contrastCoverage, ...disposition }, null, 2) + "\n");
          manifest.entries.push(capture);
        } catch (error) {
          const overlapDiagnostics = await page.evaluate(() => {
            const editor = document.querySelector(".code-editor");
            const describe = node => {
              const box = node.getBoundingClientRect(), style = getComputedStyle(node);
              return { tag: node.tagName, id: node.id, className: node.className, text: node.textContent?.slice(0, 120), rect: { x: box.x, y: box.y, width: box.width, height: box.height, left: box.left, right: box.right, top: box.top, bottom: box.bottom }, style: { position: style.position, zIndex: style.zIndex, opacity: style.opacity, color: style.color, backgroundColor: style.backgroundColor, backgroundImage: style.backgroundImage, overflow: style.overflow, overflowX: style.overflowX, overflowY: style.overflowY, transform: style.transform, contain: style.contain, isolation: style.isolation, pointerEvents: style.pointerEvents, display: style.display, visibility: style.visibility } };
            };
            const selectors = [".ace_storage", ".ace_identifier", ".ace_constant.ace_numeric", ".code-editor-hint"];
            const samples = [];
            let axeSetup = false, axeSetupError;
            try { window.axe.setup(document); axeSetup = true; }
            catch (failure) { axeSetupError = failure.message; }
            try { for (const selector of selectors) {
              const node = editor.querySelector(selector); if (!node) { samples.push({ selector, missing: true }); continue; }
              const range = document.createRange(); range.selectNodeContents(node); const textBox = range.getBoundingClientRect(), point = { x: (textBox.left + textBox.right) / 2, y: (textBox.top + textBox.bottom) / 2 };
              const ancestors = []; for (let ancestor = node; ancestor; ancestor = ancestor.parentElement) ancestors.push(describe(ancestor));
              const overlapping = [...document.body.querySelectorAll("*")].filter(candidate => { const box = candidate.getBoundingClientRect(); return box.width > 0 && box.height > 0 && point.x >= box.left && point.x <= box.right && point.y >= box.top && point.y <= box.bottom; });
              const axeColor = window.axe.commons?.color, backgroundElements = [];
              let axeStackDiagnostic;
              if (axeSetup) {
                try { axeStackDiagnostic = { invocation: "axe.commons.dom.getTextElementStack(element)", stacks: window.axe.commons.dom.getTextElementStack(node).map(stack => stack.map(describe)) }; }
                catch (failure) { axeStackDiagnostic = { invocation: "axe.commons.dom.getTextElementStack(element)", error: failure.message }; }
              } else axeStackDiagnostic = { unavailable: true, setupError: axeSetupError };
              let backgroundDiagnostic;
              if (axeSetup && typeof axeColor?.getBackgroundColor === "function") {
                try { const colour = axeColor.getBackgroundColor(node, backgroundElements); backgroundDiagnostic = { invocation: "axe.commons.color.getBackgroundColor(element, backgroundElements)", colour: colour === null ? null : { red: colour.red, green: colour.green, blue: colour.blue, alpha: colour.alpha }, backgroundElements: backgroundElements.map(describe) }; }
                catch (failure) { backgroundDiagnostic = { invocation: "axe.commons.color.getBackgroundColor(element, backgroundElements)", error: failure.message }; }
              } else backgroundDiagnostic = { unavailable: true, availableColourHelpers: Object.keys(axeColor ?? {}) };
              samples.push({ selector, point, element: describe(node), ancestors, hitStack: document.elementsFromPoint(point.x, point.y).map(describe), overlappingCount: overlapping.length, overlapping: overlapping.slice(0, 100).map(describe), axeStackDiagnostic, backgroundDiagnostic });
            } } finally { if (axeSetup) window.axe.teardown(); }
            return { samples, axeSetup, ...(axeSetupError === undefined ? {} : { axeSetupError }), scope: "Failure diagnostics only; elementsFromPoint is the browser hit stack, overlapping is a bounded rectangle inventory rather than a painted-order claim." };
          });
          await write(`${name}.overlap-diagnostics.json`, JSON.stringify(overlapDiagnostics, null, 2) + "\n");
          const geometryBeforeScreenshot = await page.evaluate(() => window.fixture.geometry());
          await write(`${name}.failure.json`, JSON.stringify({ message: error.message, dom: await page.locator("body").innerHTML(), observed: await status(), geometryBeforeScreenshot }, null, 2) + "\n");
          await page.screenshot({ path: path.join(output, `${name}.failure.png`), fullPage: true });
          await write(`${name}.failure-after-screenshot.json`, JSON.stringify(await page.evaluate(() => window.fixture.geometry()), null, 2) + "\n");
          manifest.failures.push({ name, message: error.message }); throw error;
        }
      }
      // Native Shift+Arrow selection is a separate geometry route from synthetic CSS probes.
      // The scoped Moli receipt excludes it explicitly; the identical-source Chromium receipt
      // must retain its selected text, actual marker rectangles and screenshots.
      if (scoped) manifest.exclusions.push({ check: "nativeSelectionGeometry", theme, width, reason: "Native Shift+Arrow selection geometry is excluded from this scoped Moli route; identical-source Chromium covers it." });
      else {
        const source = "const count = count;";
        for (const selectionKind of ["line", "word"]) {
          await show("default", "javascript", source); await focus(); await page.keyboard.press(`${modifier}+Home`);
          if (selectionKind === "line") await page.keyboard.press("Shift+End");
          else {
            for (let index = 0; index < 6; index++) await page.keyboard.press("ArrowRight");
            for (let index = 0; index < 5; index++) await page.keyboard.press("Shift+ArrowRight");
          }
          await render();
          const markers = await page.evaluate(() => {
            const editor = document.querySelector(".code-editor .ace_editor"), native = editor.env.editor;
            const rectangle = node => { const rect = node.getBoundingClientRect(); return { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom, width: rect.width, height: rect.height }; };
            return { selectedText: native.getSelectedText(), range: native.getSelectionRange(), markers: [...editor.querySelectorAll(".ace_marker-layer > div")].map(node => ({ className: node.className, background: getComputedStyle(node).backgroundColor, rect: rectangle(node) })) };
          });
          assert.equal(markers.selectedText, selectionKind === "line" ? source : "count", "Native selection did not select the intended source");
          const selections = markers.markers.filter(marker => marker.className.split(/\s+/).includes("ace_selection"));
          const activeLines = markers.markers.filter(marker => marker.className.split(/\s+/).includes("ace_active-line"));
          assert.ok(selections.length > 0 && activeLines.length > 0, "Native selection and active-line markers are required");
          const overlaps = (first, second) => first.left < second.right && first.right > second.left && first.top < second.bottom && first.bottom > second.top;
          assert.ok(selections.some(selection => activeLines.some(active => overlaps(selection.rect, active.rect))), "Native selection must overlap its active line");
          if (selectionKind === "word") assert.ok(markers.markers.some(marker => marker.className.split(/\s+/).includes("ace_selected-word")), "Native word selection must render selected-word markers");
          const screenshot = `${theme}-${width}-native-${selectionKind}-selection.png`;
          await page.screenshot({ path: path.join(output, screenshot), fullPage: true });
          const detail = { theme, width, nativeMarkerSelection: { kind: selectionKind, ...markers, screenshot, sha256: sha(await readFile(path.join(output, screenshot))), boundary: "Actual native marker placement; hypothetical marker overlap orders are covered separately by CSS contrast probes." } };
          manifest.interactionDetails.push(detail); await write(`${theme}-${width}-native-${selectionKind}-selection.json`, JSON.stringify(detail, null, 2) + "\n");
        }
      }
      // Each native input event is a separate admitted group. This characterizes's explicit granularity.
      await show("default", "text", ""); await focus();
      const historyValues = [];
      for (const character of ["a", "b", "c"]) { await page.keyboard.type(character); historyValues.push((await status()).value); }
      assert.deepEqual(historyValues, ["a", "ab", "abc"]);
      const undoValues = [], redoValues = [];
      for (let index = 0; index < 3; index++) { await page.keyboard.press(`${modifier}+z`); undoValues.push((await status()).value); }
      for (let index = 0; index < 3; index++) { await page.keyboard.press(`${modifier}+Shift+z`); redoValues.push((await status()).value); }
      assert.deepEqual(undoValues, ["ab", "a", ""]); assert.deepEqual(redoValues, ["a", "ab", "abc"]);
      manifest.interactionDetails.push({ theme, width, perFlushHistory: { historyValues, undoValues, redoValues } });
      const shortcutBindings = modifier === "Meta" ? [["Settings", "Meta+,"], ["Next error", "F4"], ["Previous error", "Shift+F4"], ["Find", "Meta+f"], ["Replace", "Meta+Alt+f"], ["Go to line", "Meta+l"], ["Command palette", "F1"]] : [["Settings", "Control+,"], ["Next error", "Alt+e"], ["Previous error", "Alt+Shift+e"], ["Find", "Control+f"], ["Replace", "Control+h"], ["Go to line", "Control+l"], ["Command palette", "F1"]];
      for (const readonly of [false, true]) {
        await show(readonly ? "readonly" : "default"); await focus();
        const before = await status(), requestCount = manifest.responses.length, errorCount = manifest.consoleErrors.length;
        for (const [action, binding] of shortcutBindings) for (let attempt = 1; attempt <= 2; attempt++) {
          await page.locator(".code-editor-status").evaluate(node => { node.textContent = ""; node.hidden = true; });
          assert.equal(await page.locator(".code-editor-status").textContent(), "", "Shortcut feedback did not clear before the next press");
          await page.keyboard.press(binding); await render();
          assert.equal(await page.locator(".code-editor-status").isHidden(), false, "Shortcut did not publish fresh visible feedback");
          assert.equal(await page.locator(".code-editor-status").innerText(), `${action} is unavailable in this editor.`, `Missing ${action} feedback on attempt ${attempt}, readonly=${readonly}`);
          const after = await status(); assert.equal(after.value, before.value); assert.equal(after.canUndo, before.canUndo); assert.equal(after.canRedo, before.canRedo); assert.deepEqual(after.errors, []);
          assert.equal(manifest.consoleErrors.length, errorCount, `Shortcut ${binding} produced a page error`);
          assert.equal(manifest.responses.length, requestCount, `Shortcut ${binding} requested an asset`);
        }
        manifest.interactionDetails.push({ theme, width, extensionShortcuts: { readonly, bindings: shortcutBindings, attempts: 2 } });
      }
      // Independent interaction checks operate on user input and the public component handle.
      await show(); await focus(); await page.keyboard.press(`${modifier}+End`); await page.keyboard.type("!");
      const edited = await status(); assert.ok(edited.events.length > 0); assert.equal(edited.value.endsWith("!"), true);
      await page.evaluate(() => window.fixture.echo()); assert.equal((await status()).canUndo, true); assert.equal((await status()).events.length, edited.events.length);
      await page.keyboard.press(`${modifier}+z`); assert.equal((await status()).value.endsWith("!"), false);
      await page.keyboard.press(`${modifier}+Shift+z`); assert.equal((await status()).value.endsWith("!"), true);
      await page.getByRole("button", { name: "Undo", exact: true }).click(); await type("?"); assert.equal((await status()).canRedo, false);
      await show(); await focus(); await page.keyboard.press(`${modifier}+Home`); await page.keyboard.press("ArrowDown"); await page.keyboard.press("ArrowRight"); await page.keyboard.press("ArrowRight");
      const original = (await status()).value;
      await page.evaluate(() => window.fixture.session({ sessionId: "source-b", value: "other", mode: "python" }));
      assert.equal((await status()).canUndo, false);
      await page.evaluate(value => window.fixture.session({ sessionId: "source-a", value, mode: "javascript" }), original);
      if (scoped) manifest.exclusions.push({ check: "nativeCursorRestoration", theme, width, reason: "Native cursor restoration is excluded from this scoped Moli route; Chromium must cover it at identical source." });
      else { await type("X"); assert.equal((await status()).value, original.replace("\nco", "\ncoX")); }
      await show("default", "text", "plain"); await focus(); await page.keyboard.press(`${modifier}+End`);
      await sourceInput().evaluate(node => { const data = new DataTransfer(); data.setData("text/plain", " pasted <script>literal</script>"); node.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true })); });
      await waitForStatus(value => value.value.includes(" pasted "), "Plain-text paste did not reach the component");
      assert.equal((await status()).value, "plain pasted <script>literal</script>"); assert.equal(await page.locator("#scene script").count(), 0);
      await show(); await page.locator("#before").focus(); await page.keyboard.press("Tab");
      assert.equal(await page.locator(".ace_scroller").evaluate(node => node === document.activeElement), true, "Tab did not reach the keyboard entry surface");
      await page.keyboard.press("Enter"); assert.equal(await sourceInput().evaluate(node => node === document.activeElement), true, "Enter did not enter source editing");
      await page.keyboard.press("Escape"); await page.keyboard.press("Tab");
      assert.equal(await page.locator("#after").evaluate(node => node === document.activeElement), true, "Escape and Tab did not reach the next usable control");
      await page.keyboard.press("Shift+Tab"); assert.equal(await page.locator(".ace_scroller").evaluate(node => node === document.activeElement), true, "Reverse Tab did not reach the editor surface");
      await page.keyboard.press("Enter"); assert.equal(await sourceInput().evaluate(node => node === document.activeElement), true);
      await page.keyboard.press("Escape"); await page.keyboard.press("Shift+Tab");
      assert.equal(await page.locator("#before").evaluate(node => node === document.activeElement), true, "Reverse departure did not reach the previous usable control");
      manifest.interactionDetails.push({ theme, width, keyboard: { enteredFromTab: true, enterFocusedInput: true, forwardExit: "after", reverseExit: "before" } });
      // Exercise the actual input event boundary, without claiming an OS IME or clipboard grant.
      await show("error"); await focus();
      const compositionPreflight = await sourceInput().evaluate(node => {
        const event = new InputEvent("beforeinput", { inputType: "insertCompositionText", data: "é".repeat(8), isComposing: true, bubbles: true, cancelable: true }); node.dispatchEvent(event);
        return { prevented: event.defaultPrevented, composing: event.isComposing };
      });
      assert.deepEqual(compositionPreflight, { prevented: true, composing: true });
      assert.equal((await status()).value, "ok"); assert.equal((await status()).events.length, 0);
      await renderChangedFeedback(`${theme}-${width}-composition-preflight`);
      await page.getByText("Code editor value must be at most 8 UTF-8 bytes.", { exact: true }).waitFor();
      await page.evaluate(() => window.fixture.setValue("ok"));
      assert.equal(await page.locator(".code-editor-error").isHidden(), true, "Preflight feedback must clear before probing final-input admission");
      const compositionFinal = await sourceInput().evaluate(node => {
        const before = new InputEvent("beforeinput", { inputType: "insertCompositionText", data: null, isComposing: true, bubbles: true, cancelable: true }); node.dispatchEvent(before);
        node.value = "é".repeat(8); node.dispatchEvent(new InputEvent("input", { inputType: "insertCompositionText", data: null, isComposing: false, bubbles: true }));
        return { beforePrevented: before.defaultPrevented };
      });
      assert.deepEqual(compositionFinal, { beforePrevented: false });
      await renderChangedFeedback(`${theme}-${width}-composition-final`);
      await page.locator(".code-editor-error:not([hidden])").filter({ hasText: "Code editor value must be at most 8 UTF-8 bytes." }).waitFor();
      assert.equal((await status()).value, "ok"); assert.equal((await status()).events.length, 0);
      manifest.interactionDetails.push({ theme, width, compositionAdmission: { preflight: compositionPreflight, finalInput: compositionFinal, retainedValue: "ok", changes: (await status()).events.length } });
      await show(); await focus(); const beforeDispose = (await status()).events.length; await page.evaluate(() => window.fixture.dispose()); await page.keyboard.type("late"); assert.equal((await status()).events.length, beforeDispose);
      if (width === 390) {
        await show(); const beforeTouch = (await status()).value;
        await focus(); await page.keyboard.press(`${modifier}+End`); await page.keyboard.type("!");
        const edited = (await status()).value, touchReceipt = { theme, width, beforeTouch, edited, status: "unverified", events: [] };
        const tap = async name => {
          await render(); const box = await page.getByRole("button", { name, exact: true }).boundingBox();
          assert.ok(box && box.width > 0 && box.height > 0, `Touch ${name} has no geometry`);
          const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
          const touchStart = { type: "touchStart", touchPoints: [{ ...point, id: 1 }] }, touchEnd = { type: "touchEnd", touchPoints: [] };
          try {
            const startResult = await guardCdp.send("Input.dispatchTouchEvent", touchStart);
            touchReceipt.events.push({ control: name, request: touchStart, result: startResult });
            const endResult = await guardCdp.send("Input.dispatchTouchEvent", touchEnd);
            touchReceipt.events.push({ control: name, request: touchEnd, result: endResult });
          } catch (error) { touchReceipt.failure = error.message; throw error; }
        };
        try {
          touchReceipt.enableRequest = { enabled: true, maxTouchPoints: 1 };
          touchReceipt.enableResult = await guardCdp.send("Emulation.setTouchEmulationEnabled", touchReceipt.enableRequest);
          await tap("Undo"); await waitForStatus(value => value.value === beforeTouch, "Touch Undo did not restore the previous source");
          assert.equal((await status()).canRedo, true);
          await tap("Redo"); await waitForStatus(value => value.value === edited, "Touch Redo did not restore source");
          touchReceipt.retainedValue = (await status()).value; touchReceipt.status = "passed"; manifest.checks.touchToolbar = true;
        } catch (error) { touchReceipt.failure = error.message; throw error; }
        finally {
          manifest.interactionDetails.push({ touchToolbar: touchReceipt });
          await write(`${theme}-${width}-touch-toolbar.json`, JSON.stringify(touchReceipt, null, 2) + "\n");
        }
        await guardCdp.send("Emulation.setTouchEmulationEnabled", { enabled: false });
      }
      for (const check of interactions) if (!["touchToolbar", "pointerToolbar"].includes(check)) manifest.checks[check] = !(scoped && check === "cursor");
      manifest.checks.modes = [...modes];
      await captureGuardState();
    }
    await captureGuardState();
    assert.deepEqual(unexpectedRequests, [], "Editor made external requests");
    assert.deepEqual(manifest.responses.filter(response => response.status >= 400), [], "Browser HTTP failures (URLs retained in manifest)");
    assert.deepEqual(manifest.requestFailures, [], "Browser transport failures (URLs retained in manifest)");
    assert.deepEqual(manifest.consoleErrors, [], "Browser console/page errors"); assert.deepEqual(manifest.cspViolations, [], "Strict CSP violations");
    if (scoped) { manifest.partialAcceptance = true; validateCodeEditorPartialManifest(manifest); }
    else { validateCodeEditorManifest(manifest); manifest.completeAcceptance = true; }
    await write("manifest.json", JSON.stringify(manifest) + "\n");
    console.log(JSON.stringify({ host: manifest.host, route, browser: manifest.browser, revision, entries: manifest.entries.length, scope, completeAcceptance: manifest.completeAcceptance, partialAcceptance: manifest.partialAcceptance, output }));
  } catch (error) {
    if (guardCdp) {
      try { await captureGuardState(); }
      catch (diagnosticError) { manifest.guardDiagnosticError = diagnosticError.message; }
    }
    manifest.completeAcceptance = false; if (scoped) manifest.partialAcceptance = false;
    if (!manifest.failures.some(failure => failure.message === error.message)) manifest.failures.push({ name: "interaction", message: error.message });
    await write("manifest.json", JSON.stringify(manifest) + "\n"); throw error;
  } finally {
    const cleanupErrors = [];
    try { if (guardCdp) await guardCdp.detach(); } catch (error) { cleanupErrors.push(error.message); }
    try { if (browser) await browser.close(); } catch (error) { cleanupErrors.push(error.message); }
    try { await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())); } catch (error) { cleanupErrors.push(error.message); }
    if (moli && moli.exitCode === null) { moli.kill("SIGTERM"); await Promise.race([new Promise(resolve => moli.once("exit", resolve)), new Promise(resolve => setTimeout(resolve, 3000))]); if (moli.exitCode === null) moli.kill("SIGKILL"); }
    if (moli) await write("moli.log", moliOutput);
    if (cleanupErrors.length) fail(`Browser fixture cleanup failed: ${cleanupErrors.join("; ")}`);
  }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await run();

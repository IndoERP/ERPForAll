/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { CODE_EDITOR_STATES, classifyPlaywrightEvalCaller, codeEditorBrowserRoute, codeEditorBrowserScope, validateCodeEditorManifest, validateCodeEditorPartialManifest, validateCodeEditorRouteComparison, measureCodeEditorContrastCoverage } from "../../../../web/scripts/browser-editors-code.mjs";

// This inventory deliberately lives outside the capture loop. Removing a scene from that loop
// cannot turn an incomplete evidence package into accepted browser coverage.
const requiredStates = ["default", "empty", "hover", "focus", "edited", "undo", "redo", "readonly", "disabled", "required", "invalid", "error"];
const requiredModes = ["text", "javascript", "xml", "qweb", "scss", "python", "json", "bash"];
const requiredMarkerProbes = ["ace_selection", "ace_active-line", "ace_selected-word", "ace_highlight-marker", "ace_selection+ace_highlight-marker", "ace_highlight-marker+ace_selection", "ace_active-line+ace_selection", "ace_selection+ace_active-line", "ace_selected-word+ace_selection", "ace_selection+ace_selected-word"];
function manifest() {
  return {
    schema: 1, route: "moli", browser: "fixture-browser", revision: "a".repeat(40), host: "fixture-host", generated_at: "2026-11-03T00:00:00.000Z",
    sourceFiles: [{ path: "addons/web/static/src/client/code-editor.ts", sha256: "b".repeat(64) }],
    entries: ["light", "dark"].flatMap(theme => [1280, 820, 390].flatMap(width => requiredStates.map(state => ({
      theme, width, state, screenshot: `${theme}-${width}-${state}.png`, sha256: "c".repeat(64),
      axe: `${theme}-${width}-${state}.axe.json`, violations: [], contrastIncomplete: [] as unknown[],
    })))),
    checks: { modes: requiredModes, input: true, paste: true, history: true, sessions: true, cursor: true,
      readonly: true, disabled: true, keyboardExit: true, parentEcho: true, literalSource: true,
      strictCsp: true, externalStyles: true, noExternalRequests: true, disposal: true,
      keyboardEntry: true, keyboardReverseExit: true, readonlySelectionCopy: true, readonlyEditRoutes: true, compositionAdmission: true, pointerToolbar: true, touchToolbar: true, extensionShortcuts: true, perFlushHistory: true, stylesheetComposition: true, tokenContrast: true },
    contrastSamples: ["light", "dark"].flatMap(theme => [1280, 820, 390].flatMap(width => requiredModes.map(mode => ({ theme, width, mode, tokens: [{ text: "sample", className: "ace_keyword", foreground: "rgb(255, 255, 255)", background: "rgb(0, 0, 0)", ratio: 21 }], markerTokens: requiredMarkerProbes.map(marker => ({ marker, text: "sample", ratio: 21 })) })))),
    responses: [] as { url: string; status: number; contentType: string }[], requestFailures: [] as { url: string; error: string }[], consoleErrors: [], cspViolations: [], forbiddenCalls: [], evalBypasses: [] as { sequence: number; operation: string; caller: string; stack: string }[], styleInjectionCount: 0, failures: [],
  };
}
test("code editor browser receipt requires every theme, viewport and applicable state", () => {
  expect(CODE_EDITOR_STATES).toEqual(requiredStates);
  expect(() => validateCodeEditorManifest(manifest())).not.toThrow();
  const missing = manifest(); missing.entries.pop();
  expect(() => validateCodeEditorManifest(missing)).toThrowError("Code editor evidence is missing dark/390/error.");
});
test("code editor browser receipt rejects duplicate captures and missing actual interactions", () => {
  const duplicate = manifest(); duplicate.entries.push({ ...duplicate.entries[0]! });
  expect(() => validateCodeEditorManifest(duplicate)).toThrowError("Code editor evidence repeats light/1280/default.");
  const untested = manifest(); untested.checks.paste = false;
  expect(() => validateCodeEditorManifest(untested)).toThrowError("Code editor evidence did not establish paste.");
  const modes = manifest(); modes.checks.modes = requiredModes.slice(1);
  expect(() => validateCodeEditorManifest(modes)).toThrowError("Code editor evidence did not establish mode text.");
});
test("code editor browser receipt refuses CSP, accessibility, console and source identity gaps", () => {
  const violations = manifest(); (violations.entries[0]!.violations as unknown[]).push({ id: "label" });
  expect(() => validateCodeEditorManifest(violations)).toThrowError("Code editor evidence has accessibility violations in light/1280/default.");
  const csp = manifest(); (csp.cspViolations as unknown[]).push({ blockedURI: "inline" });
  expect(() => validateCodeEditorManifest(csp)).toThrowError("Code editor evidence contains CSP violations.");
  const consoleFailure = manifest(); (consoleFailure.consoleErrors as string[]).push("engine failed");
  expect(() => validateCodeEditorManifest(consoleFailure)).toThrowError("Code editor evidence contains browser errors.");
  const unpinned = manifest(); unpinned.revision = "unverified";
  expect(() => validateCodeEditorManifest(unpinned)).toThrowError("Code editor evidence requires an exact Git revision.");
  const sources = manifest(); sources.sourceFiles = [];
  expect(() => validateCodeEditorManifest(sources)).toThrowError("Code editor evidence requires source hashes.");
});

test("code editor browser receipt refuses swallowed dynamic-code, worker or style injection attempts", () => {
  const dynamic = manifest(); (dynamic.forbiddenCalls as string[]).push("Function");
  expect(() => validateCodeEditorManifest(dynamic)).toThrowError("Code editor evidence attempted dynamic code or worker construction.");
  const styles = manifest(); styles.styleInjectionCount = 1;
  expect(() => validateCodeEditorManifest(styles)).toThrowError("Code editor evidence attempted style-element injection.");
});

const permittedEvalStack = [
  "Error: Code editor eval caller",
  "    at Object.apply (http://127.0.0.1:59117/guard.js:1:415)",
  "    at UtilityScript.evaluate (<anonymous>:290:30)",
  "    at UtilityScript.<anonymous> (<anonymous>:1:44)",
].join("\n");
test("eval exemption admits only the observed immediate Playwright caller", () => {
  expect(classifyPlaywrightEvalCaller(permittedEvalStack)).toBe("UtilityScript.evaluate (<anonymous>:290:30)");
  expect(classifyPlaywrightEvalCaller(permittedEvalStack.replace("290:30", "291:30"))).toBeNull();
  expect(classifyPlaywrightEvalCaller(permittedEvalStack.replace("UtilityScript.evaluate", "editor.evaluate"))).toBeNull();
  expect(classifyPlaywrightEvalCaller(permittedEvalStack.replace("guard.js", "fixture.js"))).toBeNull();
  expect(classifyPlaywrightEvalCaller(permittedEvalStack.replace("    at UtilityScript.evaluate", "    at editor.useEval (http://127.0.0.1:59117/fixture.js:10:2)\n    at UtilityScript.evaluate"))).toBeNull();
  expect(classifyPlaywrightEvalCaller("UtilityScript.evaluate (<anonymous>:290:30)")).toBeNull();
  expect(classifyPlaywrightEvalCaller({ stack: permittedEvalStack })).toBeNull();
});
test("browser receipt validates each eval bypass against its exact caller stack", () => {
  const admitted = manifest(); admitted.evalBypasses.push({ sequence: 1, operation: "eval", caller: "UtilityScript.evaluate (<anonymous>:290:30)", stack: permittedEvalStack });
  expect(() => validateCodeEditorManifest(admitted)).not.toThrow();
  for (const change of [
    { caller: "UtilityScript.evaluate" }, { operation: "Function" }, { sequence: 2 },
    { stack: permittedEvalStack.replace("UtilityScript.evaluate", "editor.evaluate") },
  ]) {
    const invalid = manifest(); invalid.evalBypasses.push({ ...admitted.evalBypasses[0]!, ...change });
    expect(() => validateCodeEditorManifest(invalid)).toThrowError("Code editor evidence has an invalid eval bypass ledger entry.");
  }
  const absent = manifest() as unknown as Record<string, unknown>; delete absent["evalBypasses"];
  expect(() => validateCodeEditorManifest(absent)).toThrowError("Code editor evidence requires an eval bypass ledger.");
});

test("browser receipt requires keyboard entry, both exit directions, readonly clipboard routes and composition admission", () => {
  for (const check of ["keyboardEntry", "keyboardReverseExit", "readonlySelectionCopy", "readonlyEditRoutes", "compositionAdmission", "pointerToolbar", "touchToolbar"] as const) {
    const unproved = manifest(); unproved.checks[check] = false;
    expect(() => validateCodeEditorManifest(unproved)).toThrowError(`Code editor evidence did not establish ${check}.`);
  }
});

test("code editor browser route defaults to Moli and refuses implicit fallback", () => {
  expect(codeEditorBrowserRoute(undefined)).toBe("moli");
  expect(codeEditorBrowserRoute("moli")).toBe("moli");
  expect(codeEditorBrowserRoute("chromium")).toBe("chromium");
  for (const route of [null, false, "", "chrome", "MOLI", "chromium "]) expect(() => codeEditorBrowserRoute(route)).toThrowError("CODE_BROWSER_ROUTE must be moli or chromium.");
  const absent = manifest() as unknown as Record<string, unknown>; delete absent["route"];
  expect(() => validateCodeEditorManifest(absent)).toThrowError("Code editor evidence requires an explicit browser route.");
  const version = manifest(); version.browser = "";
  expect(() => validateCodeEditorManifest(version)).toThrowError("Code editor evidence requires the actual browser version.");
});

test("code editor browser receipt refuses HTTP and transport failures with their actual URL", () => {
  const response = manifest(); response.responses.push({ url: "http://127.0.0.1:1234/main-17.png", status: 404, contentType: "text/plain" });
  expect(() => validateCodeEditorManifest(response)).toThrowError("Code editor evidence has HTTP failure 404 at http://127.0.0.1:1234/main-17.png.");
  const request = manifest(); request.requestFailures.push({ url: "http://127.0.0.1:1234/fixture.js", error: "net::ERR_CONNECTION_RESET" });
  expect(() => validateCodeEditorManifest(request)).toThrowError("Code editor evidence contains failed browser requests.");
});

// These are receipt-admission regressions, independent of browser execution.
test("browser receipt refuses omitted shortcut, history granularity, stylesheet and token contrast proof", () => {
  for (const check of ["extensionShortcuts", "perFlushHistory", "stylesheetComposition", "tokenContrast"] as const) {
    const unproved = manifest(); unproved.checks[check] = false;
    expect(() => validateCodeEditorManifest(unproved)).toThrowError(`Code editor evidence did not establish ${check}.`);
  }
});

test("browser receipt refuses editor colour-contrast incomplete findings", () => {
  const incomplete = manifest(); incomplete.entries[0]!.contrastIncomplete.push({ id: "color-contrast", target: [".code-editor .ace_keyword"] });
  expect(() => validateCodeEditorManifest(incomplete)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
});

test("browser receipt requires every mode contrast sample and rejects failed token and marker contrast", () => {
  const missing = manifest(); missing.contrastSamples.pop();
  expect(() => validateCodeEditorManifest(missing)).toThrowError("Code editor evidence is missing contrast sample dark/390/bash.");
  const token = manifest(); token.contrastSamples[0]!.tokens[0]!.ratio = 3;
  expect(() => validateCodeEditorManifest(token)).toThrowError("Code editor evidence has insufficient token contrast in light/1280/text.");
  const marker = manifest(); marker.contrastSamples[0]!.markerTokens[0]!.ratio = 1;
  expect(() => validateCodeEditorManifest(marker)).toThrowError("Code editor evidence has insufficient marker contrast in light/1280/text.");
});

const incompletePolicyMessage = "20261103T155931Z-5c827a";
function coveredIncomplete() {
  const receipt = manifest(), entry = receipt.entries[0]!;
  const target = [".ace_line:nth-child(1) > .ace_storage.ace_type"];
  const incomplete = { id: "color-contrast", target, reasons: ["bgOverlap"], failureSummary: "Background overlap diagnostic", checks: [{ id: "color-contrast", messageKey: "bgOverlap" }] };
  const measurement = { target, scope: ".code-editor .ace_text-layer", inTextLayer: true, resolvedNodes: 1, text: "const", className: "ace_storage ace_type", foreground: [0, 0, 0, 1], background: [255, 255, 255, 1], ratio: 21, minimumRatio: 4.5, variants: [{ variant: "base", foreground: [0, 0, 0, 1], background: [255, 255, 255, 1], ratio: 21 }] };
  entry.contrastIncomplete.push(incomplete);
  Object.assign(entry, { contrastCoverage: [measurement], contrastExcused: [{ target, reasons: incomplete.reasons, policyMessage: incompletePolicyMessage }] });
  return { receipt, entry, incomplete, measurement };
}

test("browser receipt admits only lead-authorized incomplete reasons covered on the same text-layer node", () => {
  const covered = coveredIncomplete();
  expect(() => validateCodeEditorManifest(covered.receipt)).not.toThrow();
  covered.incomplete.reasons = ["shortTextContent"];
  covered.incomplete.checks[0]!.messageKey = "shortTextContent";
  (covered.entry as unknown as { contrastExcused: { reasons: string[] }[] }).contrastExcused[0]!.reasons = ["shortTextContent"];
  expect(() => validateCodeEditorManifest(covered.receipt)).not.toThrow();
});

test("browser receipt rejects unsupported or mixed incomplete reasons even with measured contrast", () => {
  for (const reasons of [["bgImage"], ["bgOverlap", "bgImage"], [], ["shortTextContent", "equalRatio"]]) {
    const covered = coveredIncomplete(); covered.incomplete.reasons = reasons; covered.incomplete.checks = reasons.map(messageKey => ({ id: "color-contrast", messageKey }));
    expect(() => validateCodeEditorManifest(covered.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
  }
});

test("browser receipt rejects incomplete nodes outside the text layer or without exact capture coverage", () => {
  for (const mutate of [
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.inTextLayer = false; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.scope = ".code-editor"; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.target = [".another-token"]; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.incomplete.target = [".code-editor-hint"]; },
    (entry: ReturnType<typeof coveredIncomplete>) => { Object.assign(entry.entry, { contrastCoverage: [] }); },
  ]) {
    const covered = coveredIncomplete(); mutate(covered);
    expect(() => validateCodeEditorManifest(covered.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
  }
});

test("browser receipt rejects low nonfinite or missing node contrast and unrecorded policy exceptions", () => {
  for (const mutate of [
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.ratio = 4.49; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.ratio = NaN; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.ratio = Infinity; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.minimumRatio = 3; },
    (entry: ReturnType<typeof coveredIncomplete>) => { Object.assign(entry.entry, { contrastExcused: [] }); },
    (entry: ReturnType<typeof coveredIncomplete>) => { Object.assign(entry.entry, { contrastExcused: [{ target: entry.incomplete.target, reasons: entry.incomplete.reasons, policyMessage: "wrong-message" }] }); },
  ]) {
    const covered = coveredIncomplete(); mutate(covered);
    expect(() => validateCodeEditorManifest(covered.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
  }
});


// These controlled DOM/Range fixtures test refusal policy in the exact serialized browser
// function. They establish neither browser geometry nor actual rendered acceptance.
afterEach(() => { vi.restoreAllMocks(); document.body.replaceChildren(); document.documentElement.removeAttribute("style"); document.body.removeAttribute("style"); });
function contrastDom() {
  const paintProperties: Readonly<Record<string, { readonly inline: string; readonly neutral: string }>> = {
    "background-image": { inline: "backgroundImage", neutral: "none" }, "background-blend-mode": { inline: "backgroundBlendMode", neutral: "normal" },
    "mix-blend-mode": { inline: "mixBlendMode", neutral: "normal" }, filter: { inline: "filter", neutral: "none" },
    "backdrop-filter": { inline: "backdropFilter", neutral: "none" }, "-webkit-backdrop-filter": { inline: "webkitBackdropFilter", neutral: "none" },
    "mask-image": { inline: "maskImage", neutral: "none" }, "-webkit-mask-image": { inline: "webkitMaskImage", neutral: "none" }, "text-shadow": { inline: "textShadow", neutral: "none" },
  };
  document.body.innerHTML = '<section class="code-editor"><div class="ace_editor"><div class="ace_content"><div class="ace_marker-layer"><div class="ace_selection"></div></div><div class="ace_text-layer"><div class="ace_line"><span id="coverage-token" class="ace_keyword">source</span></div></div></div></div></section>';
  for (const node of document.querySelectorAll<HTMLElement>("html, body, body *")) {
    node.style.color = "rgb(0, 0, 0)"; node.style.backgroundColor = "rgba(0, 0, 0, 0)"; node.style.backgroundImage = "none"; node.style.backgroundBlendMode = "normal"; node.style.mixBlendMode = "normal"; node.style.filter = "none"; node.style.opacity = "1";
    node.style.display = "block"; node.style.visibility = "visible";
    for (const property of Object.values(paintProperties)) Reflect.set(node.style, property.inline, property.neutral);
  }
  document.body.style.backgroundColor = "rgb(255, 255, 255)";
  const token = document.querySelector<HTMLElement>("#coverage-token")!, content = document.querySelector<HTMLElement>(".ace_content")!, marker = document.querySelector<HTMLElement>(".ace_selection")!, markerLayer = document.querySelector<HTMLElement>(".ace_marker-layer")!;
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(() => new DOMRect(0, 0, 100, 20));
  vi.spyOn(document, "createRange").mockImplementation(() => ({ selectNodeContents() {}, getClientRects: () => [new DOMRect(0, 0, 50, 20)] }) as unknown as Range);
  const nativeComputedStyle = globalThis.getComputedStyle.bind(window);
  vi.spyOn(globalThis, "getComputedStyle").mockImplementation((node, pseudo) => {
    const style = nativeComputedStyle(node, pseudo);
    return new Proxy(style, { get(target, key) {
      if (key === "getPropertyValue") return (name: string) => {
        // This is a controlled paint-policy model. Read its explicitly assigned inline CSS
        // before jsdom's computed normalization (notably text-shadow:none becomes transparent rgba).
        const property = paintProperties[name];
        if (property && node instanceof HTMLElement) {
          const inline: unknown = Reflect.get(node.style, property.inline); if (typeof inline === "string" && inline !== "") return inline;
        }
        return target.getPropertyValue(name);
      };
      const member: unknown = Reflect.get(target, key); return typeof member === "function" ? member.bind(target) : member;
    } });
  });
  const measure = () => measureCodeEditorContrastCoverage([{ target: ["#coverage-token"] }])[0]!;
  expect(measure(), "Neutral scoped DOM fixture must establish its black-on-white control before any paint mutation").toMatchObject({ ratio: 21 });
  return { token, content, marker, markerLayer, measure };
}

test("same-node contrast coverage refuses an intersecting transparent image-only marker", () => {
  const fixture = contrastDom(); fixture.marker.style.backgroundImage = "linear-gradient(black, white)";
  const measured = fixture.measure();
  expect(measured.ratio).toBeUndefined(); expect(measured.reason).toBe("Intersecting marker uses unsupported paint effects.");
});

test("same-node contrast coverage refuses node and ancestor image blend and filter effects", () => {
  for (const target of ["token", "content"] as const) for (const effect of ["backgroundImage", "mixBlendMode", "filter"] as const) {
    const fixture = contrastDom(); fixture[target].style[effect] = effect === "backgroundImage" ? "linear-gradient(black, white)" : effect === "mixBlendMode" ? "multiply" : "blur(1px)";
    const measured = fixture.measure();
    expect(measured.ratio).toBeUndefined(); expect(measured.reason).toBe("Node or ancestor uses unsupported paint effects.");
    vi.restoreAllMocks();
  }
});

test("same-node contrast coverage refuses marker-layer paint and opacity effects", () => {
  for (const effect of ["backgroundImage", "mixBlendMode", "filter", "opacity"] as const) {
    const fixture = contrastDom(); fixture.marker.style.backgroundColor = "rgba(0, 0, 0, 0.1)";
    fixture.markerLayer.style[effect] = effect === "backgroundImage" ? "linear-gradient(black, white)" : effect === "mixBlendMode" ? "multiply" : effect === "filter" ? "blur(1px)" : "0.5";
    const measured = fixture.measure();
    expect(measured.ratio).toBeUndefined(); expect(measured.reason).toBe("Marker layer or ancestor uses unsupported paint effects.");
    vi.restoreAllMocks();
  }
});


test("same-node contrast receipt refuses forged ratios colours and duplicate target coverage", () => {
  for (const mutate of [
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.ratio = 22; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.variants[0]!.ratio = 22; },
    (entry: ReturnType<typeof coveredIncomplete>) => { entry.measurement.foreground = [255, 255, 255, 1]; },
    (entry: ReturnType<typeof coveredIncomplete>) => { Object.assign(entry.entry, { contrastCoverage: [entry.measurement, entry.measurement] }); },
  ]) {
    const covered = coveredIncomplete(); mutate(covered);
    expect(() => validateCodeEditorManifest(covered.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
  }
});


test("same-node contrast coverage refuses every unsupported paint effect on an empty intersecting marker layer", () => {
  for (const [property, value] of [
    ["background-image", "linear-gradient(black, white)"], ["background-blend-mode", "multiply"], ["mix-blend-mode", "multiply"],
    ["filter", "blur(1px)"], ["backdrop-filter", "blur(1px)"], ["mask-image", "linear-gradient(black, white)"],
    ["-webkit-mask-image", "linear-gradient(black, white)"], ["opacity", "0.5"], ["background-color", "rgb(0, 0, 0)"],
  ]) {
    const fixture = contrastDom(); fixture.marker.remove();
    // jsdom does not implement every optional paint property. Supply its exact computed value
    // only on this layer; the shared measurement and all other computed styles remain unchanged.
    // Capture the scoped implementation itself: binding the existing spy would recurse
    // when mockImplementation replaces that same spy's implementation below.
    const scopedComputedStyle = vi.mocked(globalThis.getComputedStyle).getMockImplementation();
    if (!scopedComputedStyle) throw new Error("The scoped contrast CSS fixture is not installed.");
    vi.mocked(globalThis.getComputedStyle).mockImplementation((node, pseudo) => {
      const style = scopedComputedStyle(node, pseudo);
      if (node !== fixture.markerLayer) return style;
      return new Proxy(style, { get(target, key) {
        if (key === "getPropertyValue") return (name: string) => name === property ? value : target.getPropertyValue(name);
        if (key === "opacity" && property === "opacity" || key === "backgroundColor" && property === "background-color") return value;
        const member: unknown = Reflect.get(target, key); return typeof member === "function" ? member.bind(target) : member;
      } });
    });
    const measured = fixture.measure();
    expect(measured.ratio, property).toBeUndefined(); expect(measured.reason, property).toBe("Marker layer or ancestor uses unsupported paint effects.");
    vi.restoreAllMocks();
  }
});

test("same-node contrast receipt refuses contradictory unsupported witness and raw reason data", () => {
  const refused = coveredIncomplete(); Object.assign(refused.measurement, { reason: "Unsupported paint effects.", witness: { filter: "blur(1px)" } });
  expect(() => validateCodeEditorManifest(refused.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
  const contradictory = coveredIncomplete(); Object.assign(contradictory.incomplete.checks[0]!, { data: { messageKey: "bgImage" } });
  expect(() => validateCodeEditorManifest(contradictory.receipt)).toThrowError("Code editor evidence has incomplete editor colour contrast in light/1280/default.");
});


test("mode contrast receipts require the highlight marker and both highlight-selection compositions", () => {
  for (const required of requiredMarkerProbes.slice(2)) {
    const receipt = manifest(); receipt.contrastSamples[0]!.markerTokens = receipt.contrastSamples[0]!.markerTokens.filter(token => token.marker !== required);
    expect(() => validateCodeEditorManifest(receipt)).toThrowError("Code editor evidence has insufficient marker contrast in light/1280/text.");
  }
});


test("mode contrast receipt refuses missing or unsafe active-line and selected-word selection overlaps", () => {
  for (const marker of ["ace_active-line", "ace_active-line+ace_selection", "ace_selection+ace_active-line", "ace_selected-word+ace_selection", "ace_selection+ace_selected-word"]) {
    const missing = manifest(); missing.contrastSamples[0]!.markerTokens = missing.contrastSamples[0]!.markerTokens.filter(token => token.marker !== marker);
    expect(() => validateCodeEditorManifest(missing)).toThrowError("Code editor evidence has insufficient marker contrast in light/1280/text.");
    for (const ratio of [4.49, NaN, Infinity]) {
      const unsafe = manifest(); unsafe.contrastSamples[0]!.markerTokens.find(token => token.marker === marker)!.ratio = ratio;
      expect(() => validateCodeEditorManifest(unsafe)).toThrowError("Code editor evidence has insufficient marker contrast in light/1280/text.");
    }
  }
});

test("full browser acceptance refuses a receipt labelled as scoped partial evidence", () => {
  const partial = { ...manifest(), scope: "moli-native-selection-excluded", acceptance: "partial", completeAcceptance: false, excludedChecks: ["sourceGeometry", "caretGeometry", "nativeCursorRestoration", "nativeSelectionGeometry"] };
  const oldProbes = ["ace_selection", "ace_selected-word", "ace_highlight-marker", "ace_selection+ace_highlight-marker", "ace_highlight-marker+ace_selection"];
  for (const sample of partial.contrastSamples) sample.markerTokens = sample.markerTokens.filter(token => oldProbes.includes(token.marker));
  expect(() => validateCodeEditorManifest(partial)).toThrowError("Partial code editor evidence cannot establish full browser acceptance.");
});


test("legacy marker receipt cannot omit every active-line and selection-word overlap probe", () => {
  const legacy = manifest();
  const oldProbes = ["ace_selection", "ace_selected-word", "ace_highlight-marker", "ace_selection+ace_highlight-marker", "ace_highlight-marker+ace_selection"];
  for (const sample of legacy.contrastSamples) sample.markerTokens = sample.markerTokens.filter(token => oldProbes.includes(token.marker));
  expect(() => validateCodeEditorManifest(legacy)).toThrowError("Code editor evidence has insufficient marker contrast in light/1280/text.");
});


function scopedManifest() {
  return { ...manifest(), scope: "moli-native-selection-excluded", acceptance: "partial", completeAcceptance: false, partialAcceptance: true,
    excludedChecks: ["sourceGeometry", "caretGeometry", "nativeCursorRestoration", "nativeSelectionGeometry"], checks: { ...manifest().checks, cursor: false },
    exclusions: [{ check: "sourceGeometry", theme: "light", width: 1280, state: "default", reason: "Measured source/caret layout disagreement.", observedFailure: true, observed: { firstToken: { x: 0, width: 0 } } }, { check: "nativeCursorRestoration", theme: "light", width: 1280, reason: "Native cursor route requires same-source Chromium proof." }],
  };
}
test("partial Moli scope is explicit and unavailable to an implicit native fallback", () => {
  expect(codeEditorBrowserScope(undefined, "moli")).toBe("full");
  expect(codeEditorBrowserScope("full", "chromium")).toBe("full");
  expect(codeEditorBrowserScope("moli-native-selection-excluded", "moli")).toBe("moli-native-selection-excluded");
  for (const value of ["partial", "", null, false, "moli-native-selection-excluded"]) expect(() => codeEditorBrowserScope(value, "chromium")).toThrowError("CODE_BROWSER_SCOPE must be full, or moli-native-selection-excluded with the moli route.");
});
test("partial Moli admission preserves unrelated failures and requires measured narrow exclusions", () => {
  expect(() => validateCodeEditorPartialManifest(scopedManifest())).not.toThrow();
  for (const patch of [{ route: "chromium" }, { completeAcceptance: true }, { partialAcceptance: false }, { acceptance: "passed" }]) expect(() => validateCodeEditorPartialManifest({ ...scopedManifest(), ...patch })).toThrowError("Scoped code editor evidence must be explicitly partial Moli evidence.");
  const unknown = scopedManifest(); unknown.exclusions[0]!.check = "CSP";
  expect(() => validateCodeEditorPartialManifest(unknown)).toThrowError("Scoped code editor evidence contains an unsupported exclusion.");
  const noMeasurement = scopedManifest(); noMeasurement.exclusions[0]!.observedFailure = false;
  expect(() => validateCodeEditorPartialManifest(noMeasurement)).toThrowError("Scoped geometry exclusion requires its observed failing measurement.");
  const unrelated = scopedManifest(); (unrelated.consoleErrors as string[]).push("engine failure");
  expect(() => validateCodeEditorPartialManifest(unrelated)).toThrowError("Code editor evidence contains browser errors.");
  const cursor = scopedManifest(); cursor.checks.cursor = true;
  expect(() => validateCodeEditorPartialManifest(cursor)).toThrowError("Scoped code editor evidence must leave native cursor restoration unestablished.");
  const missingCheck = scopedManifest(); missingCheck.checks.paste = false;
  expect(() => validateCodeEditorPartialManifest(missingCheck)).toThrowError("Code editor evidence did not establish paste.");
});
test("route comparison requires full native proof and identical source fixture styles and policy", () => {
  const inputs = { policy: "strict-fixture-csp", fixture_entry_sha256: "d".repeat(64), javascript_sha256: "e".repeat(64), stylesheetComposition: { sha256: "f".repeat(64), inputs: [] } };
  const native = { ...manifest(), ...inputs, route: "chromium", scope: "full", completeAcceptance: true }, partial = { ...scopedManifest(), ...inputs };
  expect(() => validateCodeEditorRouteComparison(native, partial)).not.toThrow();
  for (const key of ["revision", "policy", "fixture_entry_sha256", "javascript_sha256", "sourceFiles", "stylesheetComposition"]) {
    const changed = { ...partial, [key]: key === "revision" ? "f".repeat(40) : key === "sourceFiles" ? [{ path: "different", sha256: "a".repeat(64) }] : key === "stylesheetComposition" ? {} : "different" };
    expect(() => validateCodeEditorRouteComparison(native, changed)).toThrowError(`Code editor route comparison differs in ${key}.`);
  }
  expect(() => validateCodeEditorRouteComparison({ ...native, completeAcceptance: false }, partial)).toThrowError("Code editor route comparison requires complete native Chromium acceptance.");
});
test("versioned contrast receipts require both inherited auto and explicit presentation inventories", () => {
  const receipt = { ...manifest(), contrastPresentationVersion: 1 };
  expect(() => validateCodeEditorManifest(receipt)).toThrowError("Code editor evidence is missing auto presentation contrast in light/1280/text.");
  for (const sample of receipt.contrastSamples) {
    sample.tokens = ["auto", "explicit"].flatMap(presentation => sample.tokens.map(token => ({ ...token, presentation })));
    sample.markerTokens = ["auto", "explicit"].flatMap(presentation => sample.markerTokens.map(token => ({ ...token, presentation })));
  }
  expect(() => validateCodeEditorManifest(receipt)).not.toThrow();
  receipt.contrastSamples[0]!.markerTokens = receipt.contrastSamples[0]!.markerTokens.filter(token => Reflect.get(token, "presentation") === "auto");
  expect(() => validateCodeEditorManifest(receipt)).toThrowError("Code editor evidence is missing explicit presentation contrast in light/1280/text.");
});

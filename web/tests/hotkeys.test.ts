/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createHotkeyBoundary, createHotkeyController, UIError } from "../src/index.js";
import { uiError } from "./error-contract.js";

type Binding = {
  readonly chord: string;
  readonly target: HTMLElement;
  readonly allowEditable?: boolean;
  readonly allowRepeat?: boolean;
  readonly hint?: string;
} & ({ readonly kind: "dom" } | { readonly kind: "callback"; readonly run: () => unknown });
const cleanups: (() => void)[] = [];
afterEach(() => {
  for (const dispose of cleanups.splice(0).reverse()) dispose();
  vi.restoreAllMocks(); vi.unstubAllGlobals();
  document.body.replaceChildren();
});
function root(parent: HTMLElement = document.body): HTMLDivElement {
  const node = document.createElement("div"); parent.append(node); return node;
}
function button(parent: HTMLElement = document.body): HTMLButtonElement {
  const node = document.createElement("button"); node.textContent = "Execute"; parent.append(node); return node;
}
function key(target: EventTarget, init: KeyboardEventInit): KeyboardEvent {
  const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event); return event;
}
function access(target: EventTarget = document.body, init: KeyboardEventInit = {}): KeyboardEvent {
  return key(target, { key: "x", code: "KeyX", altKey: true, ...init });
}
function owner(host: HTMLElement, bindings: () => readonly Binding[], blocked?: () => boolean) {
  const report = vi.fn<(error: unknown) => void>();
  const controller = createHotkeyController(host, { bindings, report, ...(blocked ? { blocked } : {}) });
  cleanups.push(controller.dispose); return { ...controller, report };
}
function boundary(host: HTMLElement, onEscape?: () => unknown) {
  const report = vi.fn<(error: unknown) => void>();
  const handle = createHotkeyBoundary(host, { report, ...(onEscape ? { onEscape } : {}) });
  cleanups.push(handle.dispose); return { ...handle, report };
}
const hints = (): Element[] => [...document.querySelectorAll("[data-hotkey-hint]")];
const alt = (): void => { key(document.body, { key: "Alt", code: "AltLeft", altKey: true }); };
const mutationDelivery = async (): Promise<void> => { await Promise.resolve(); await Promise.resolve(); };

/// (partial: runtime shared ownership and native focus/click; browser proof is separate)
test("one shared dispatch focuses and clicks a selected DOM command exactly once", () => {
  const host = root(), target = button(host), click = vi.fn(); target.addEventListener("click", click);
  const first = owner(host, () => [{ kind: "dom", chord: "alt+x", target }]);
  const unrelated = owner(host, () => []);
  expect(access().defaultPrevented).toBe(true);
  expect(click).toHaveBeenCalledOnce(); expect(document.activeElement).toBe(target);
  expect(first.report).not.toHaveBeenCalled(); expect(unrelated.report).not.toHaveBeenCalled();
  expect(access(document.body, { key: "y", code: "KeyY" }).defaultPrevented).toBe(false);
});

test("duplicate eligible shortcuts across owners execute neither command and report one typed conflict", () => {
  const host = root(), a = button(host), b = button(host), clickA = vi.fn(), clickB = vi.fn();
  a.addEventListener("click", clickA); b.addEventListener("click", clickB);
  const first = owner(host, () => [{ kind: "dom", chord: "alt+x", target: a }]);
  const second = owner(host, () => [{ kind: "dom", chord: "alt+x", target: b }]);
  expect(access().defaultPrevented).toBe(true);
  expect(clickA).not.toHaveBeenCalled(); expect(clickB).not.toHaveBeenCalled();
  const errors = [...first.report.mock.calls, ...second.report.mock.calls].map(([error]) => error);
  expect(errors).toHaveLength(1); expect(errors[0]).toBeInstanceOf(UIError);
  expect(errors[0]).toMatchObject({ code: "value", message: "Duplicate active hotkey: alt+x." });
  b.disabled = true; access(); expect(clickA).toHaveBeenCalledOnce();
});

test("blocked owners exclude their commands and an unmatched event retains its native default", () => {
  const host = root(), target = button(host), run = vi.fn(); let blocked = true;
  owner(host, () => [{ kind: "callback", chord: "alt+x", target, run }], () => blocked);
  expect(access().defaultPrevented).toBe(false); expect(run).not.toHaveBeenCalled();
  blocked = false; expect(access().defaultPrevented).toBe(true); expect(run).toHaveBeenCalledOnce();
});

test("exact modifiers and physical positions preserve shifted digits and Option-produced letters", () => {
  const host = root(), target = button(host), calls: string[] = [];
  owner(host, () => ["alt+x", "alt+shift+1", "alt+7", "ctrl+enter"].map(chord => ({
    kind: "callback" as const, chord, target, run: () => { calls.push(chord); }
  })));
  access(document.body, { key: "≈" });
  access(document.body, { key: "!", code: "Digit1", shiftKey: true });
  access(document.body, { key: "7", code: "Numpad7" });
  key(document.body, { key: "Enter", code: "Enter", ctrlKey: true });
  expect(calls).toEqual(["alt+x", "alt+shift+1", "alt+7", "ctrl+enter"]);
  for (const extra of [{ ctrlKey: true }, { metaKey: true }, { shiftKey: true }, { altKey: false }]) {
    expect(access(document.body, extra).defaultPrevented).toBe(false);
  }
  expect(access(document.body, { key: "x", code: "F1" }).defaultPrevented).toBe(false);
  access(document.body, { code: "" }); expect(calls.at(-1)).toBe("alt+x"); expect(calls).toHaveLength(5);
});

test("consumed keys, IME sessions, composing events and AltGraph never invoke a command", () => {
  const host = root(), target = button(host), run = vi.fn();
  owner(host, () => [{ kind: "callback", chord: "alt+x", target, run, allowEditable: true }]);
  const consumed = new KeyboardEvent("keydown", { key: "x", code: "KeyX", altKey: true, bubbles: true, cancelable: true });
  consumed.preventDefault(); document.body.dispatchEvent(consumed);
  access(document.body, { isComposing: true });
  document.body.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  expect(access().defaultPrevented).toBe(false);
  document.body.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
  const altGraph = new KeyboardEvent("keydown", { key: "x", code: "KeyX", altKey: true, bubbles: true, cancelable: true });
  Object.defineProperty(altGraph, "getModifierState", { value: (name: string): boolean => name === "AltGraph" });
  document.body.dispatchEvent(altGraph); expect(altGraph.defaultPrevented).toBe(false);
  expect(run).not.toHaveBeenCalled(); access(); expect(run).toHaveBeenCalledOnce();
});

test("repeats are inert by default and only an explicitly repeatable binding runs repeatedly", () => {
  const host = root(), target = button(host), run = vi.fn(); let allowRepeat = false;
  owner(host, () => [{ kind: "callback", chord: "alt+x", target, run, allowRepeat }]);
  access(); expect(access(document.body, { repeat: true }).defaultPrevented).toBe(false);
  expect(run).toHaveBeenCalledOnce(); allowRepeat = true; access(document.body, { repeat: true });
  expect(run).toHaveBeenCalledTimes(2);
});

test("editable descendants are excluded unless the binding opts in, including plaintext-only inheritance", () => {
  const host = root(), target = button(host), run = vi.fn(); let allowEditable = false;
  owner(host, () => [{ kind: "callback", chord: "alt+x", target, run, allowEditable }]);
  const sources: HTMLElement[] = [];
  for (const tag of ["input", "textarea", "select"] as const) { const node = document.createElement(tag); host.append(node); sources.push(node); }
  for (const value of ["true", "", "plaintext-only"]) {
    const editor = root(host); editor.setAttribute("contenteditable", value); const child = document.createElement("span"); editor.append(child); sources.push(child);
  }
  for (const source of sources) expect(access(source).defaultPrevented).toBe(false);
  expect(run).not.toHaveBeenCalled(); allowEditable = true;
  for (const source of sources) expect(access(source).defaultPrevented).toBe(true);
  expect(run).toHaveBeenCalledTimes(sources.length);
  const editable = root(host); editable.contentEditable = "true";
  const noneditable = root(editable); noneditable.setAttribute("contenteditable", "false");
  allowEditable = false; access(noneditable); expect(run).toHaveBeenCalledTimes(sources.length + 1);
});

test("eligibility rechecks disabled, aria, hidden, inert, CSS, collapsed details and disconnected controls", () => {
  const host = root(), target = button(host), click = vi.fn(); target.addEventListener("click", click);
  owner(host, () => [{ kind: "dom", chord: "alt+x", target }]);
  const checks: (() => () => void)[] = [
    () => { target.disabled = true; return () => { target.disabled = false; }; },
    () => { target.setAttribute("aria-disabled", "true"); return () => target.removeAttribute("aria-disabled"); },
    () => { host.hidden = true; return () => { host.hidden = false; }; },
    () => { host.setAttribute("inert", ""); return () => host.removeAttribute("inert"); },
    () => { host.setAttribute("aria-hidden", "true"); return () => host.removeAttribute("aria-hidden"); },
    () => { host.style.display = "none"; return () => { host.style.display = ""; }; },
    () => { host.style.visibility = "hidden"; return () => { host.style.visibility = ""; }; },
    () => { host.style.visibility = "collapse"; return () => { host.style.visibility = ""; }; },
    () => { const fieldset = document.createElement("fieldset"); fieldset.disabled = true; host.append(fieldset); fieldset.append(target); return () => { host.append(target); fieldset.remove(); }; },
    () => { const details = document.createElement("details"); details.append(document.createElement("summary")); host.append(details); details.append(target); return () => { host.append(target); details.remove(); }; },
    () => { target.remove(); return () => { host.append(target); }; }
  ];
  for (const makeUnavailable of checks) {
    const restore = makeUnavailable(); expect(access().defaultPrevented).toBe(false); alt(); expect(hints()).toHaveLength(0); restore();
  }
  expect(click).not.toHaveBeenCalled(); access(); expect(click).toHaveBeenCalledOnce();
});

test("nested exclusive boundaries suppress background commands even when unmatched or blocked", () => {
  const host = root(), pageButton = button(host), outer = root(host), outerButton = button(outer), inner = root(outer), innerButton = button(inner);
  const pageRun = vi.fn(), outerRun = vi.fn(), innerRun = vi.fn(); let innerBlocked = false;
  owner(host, () => [{ kind: "callback", chord: "alt+x", target: pageButton, run: pageRun }]);
  boundary(outer);
  owner(outer, () => [{ kind: "callback", chord: "alt+x", target: outerButton, run: outerRun }]);
  const child = boundary(inner);
  const innerOwner = owner(inner, () => [{ kind: "callback", chord: "alt+y", target: innerButton, run: innerRun }], () => innerBlocked);
  expect(access().defaultPrevented).toBe(false); expect(pageRun).not.toHaveBeenCalled(); expect(outerRun).not.toHaveBeenCalled();
  access(document.body, { key: "y", code: "KeyY" }); expect(innerRun).toHaveBeenCalledOnce();
  innerBlocked = true; expect(access(document.body, { key: "y", code: "KeyY" }).defaultPrevented).toBe(false);
  expect(innerRun).toHaveBeenCalledOnce(); child.dispose(); innerOwner.dispose();
  access(); expect(outerRun).toHaveBeenCalledOnce(); expect(pageRun).not.toHaveBeenCalled();
});

test("a native child modal outranks an older popover even if the popover boundary registers later", () => {
  const page = root(), pageButton = button(page), pageRun = vi.fn();
  owner(page, () => [{ kind: "callback", chord: "alt+x", target: pageButton, run: pageRun }]);
  const parentDialog = document.createElement("dialog"); parentDialog.open = true; page.append(parentDialog);
  const popover = root(parentDialog), popoverButton = button(popover), popoverRun = vi.fn();
  const childDialog = document.createElement("dialog"); childDialog.open = true; document.body.append(childDialog);
  const childButton = button(childDialog), childRun = vi.fn();
  boundary(parentDialog); const child = boundary(childDialog); boundary(popover);
  owner(popover, () => [{ kind: "callback", chord: "alt+x", target: popoverButton, run: popoverRun }]);
  owner(childDialog, () => [{ kind: "callback", chord: "alt+x", target: childButton, run: childRun }]);
  access(childButton); expect(childRun).toHaveBeenCalledOnce(); expect(popoverRun).not.toHaveBeenCalled(); expect(pageRun).not.toHaveBeenCalled();
  child.dispose(); childDialog.remove(); access(popoverButton); expect(popoverRun).toHaveBeenCalledOnce();
});

test("an unregistered native modal blocks page shortcuts rather than leaking to the background", () => {
  const host = root(), target = button(host), run = vi.fn();
  owner(host, () => [{ kind: "callback", chord: "alt+x", target, run }]);
  const modal = document.createElement("dialog"); modal.open = true; document.body.append(modal); const input = button(modal);
  expect(access(input).defaultPrevented).toBe(false); expect(run).not.toHaveBeenCalled(); modal.remove();
  access(); expect(run).toHaveBeenCalledOnce();
});

test("one Escape belongs to the newest boundary and a local consumed Escape is left alone", () => {
  const host = root(), outer = root(host), outerEscape = vi.fn(), innerEscape = vi.fn();
  boundary(outer, outerEscape); const inner = root(outer), top = boundary(inner, innerEscape);
  const local = button(inner), stop = (event: KeyboardEvent): void => event.preventDefault();
  local.addEventListener("keydown", stop, { once: true });
  key(local, { key: "Escape", code: "Escape" }); expect(innerEscape).not.toHaveBeenCalled();
  expect(key(local, { key: "Escape", code: "Escape" }).defaultPrevented).toBe(true);
  expect(innerEscape).toHaveBeenCalledOnce(); expect(outerEscape).not.toHaveBeenCalled();
  top.dispose(); key(outer, { key: "Escape", code: "Escape" }); expect(outerEscape).toHaveBeenCalledOnce();
});

test("hints are text-only, aria-hidden and limited to the active eligible targets", async () => {
  const host = root(), target = button(host), unavailable = button(host); unavailable.disabled = true;
  owner(host, () => [
    { kind: "dom", chord: "alt+x", target, hint: "<img src=x>" },
    { kind: "dom", chord: "alt+y", target: unavailable }
  ]);
  alt(); expect(hints()).toHaveLength(1); expect(hints()[0]?.textContent).toBe("<img src=x>");
  expect(hints()[0]?.getAttribute("aria-hidden")).toBe("true"); expect(hints()[0]?.querySelector("img")).toBeNull();
  expect(hints()[0]?.hasAttribute("tabindex")).toBe(false);
  target.disabled = true; await mutationDelivery(); expect(hints()).toHaveLength(0);
});

test("modifier release, blur, visibility loss, refresh, overlay changes and disposal remove hints", () => {
  const host = root(), target = button(host); const controller = owner(host, () => [{ kind: "dom", chord: "alt+x", target }]);
  alt(); expect(hints()).toHaveLength(1);
  document.body.dispatchEvent(new KeyboardEvent("keyup", { key: "Alt", code: "AltLeft", bubbles: true })); expect(hints()).toHaveLength(0);
  alt(); window.dispatchEvent(new Event("blur")); expect(hints()).toHaveLength(0);
  alt(); const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange")); expect(hints()).toHaveLength(0); visibility.mockRestore();
  alt(); controller.refresh(); expect(hints()).toHaveLength(0);
  alt(); const overlay = boundary(root(host)); expect(hints()).toHaveLength(0); overlay.dispose();
  alt(); controller.dispose(); expect(hints()).toHaveLength(0); controller.dispose();
});

test("dynamic rerender suppliers drop stale targets and refresh after disposal is a usage error", () => {
  const host = root(), old = button(host), oldClick = vi.fn(), replacementClick = vi.fn(); old.addEventListener("click", oldClick);
  let current = old; const controller = owner(host, () => [{ kind: "dom", chord: "alt+x", target: current }]);
  access(); old.remove(); current = button(host); current.addEventListener("click", replacementClick); controller.refresh();
  access(); expect(oldClick).toHaveBeenCalledOnce(); expect(replacementClick).toHaveBeenCalledOnce();
  controller.dispose(); access(); expect(replacementClick).toHaveBeenCalledOnce();
  uiError(() => controller.refresh(), "usage", "Hotkey controller is disposed");
  const next = owner(host, () => [{ kind: "dom", chord: "alt+x", target: current }]); access();
  expect(replacementClick).toHaveBeenCalledTimes(2); expect(next.report).not.toHaveBeenCalled();
});

test("callback throws and late rejection after disposal reach the originating reporter", async () => {
  const host = root(), target = button(host), synchronous = new Error("command failed"), late = new Error("late command failed");
  let reject!: (error: unknown) => void;
  const pending = new Promise<never>((_resolve, no) => { reject = no; });
  let throws = true;
  const controller = owner(host, () => [{ kind: "callback", chord: "alt+x", target, run: () => { if (throws) throw synchronous; return pending; } }]);
  access(); expect(controller.report).toHaveBeenCalledExactlyOnceWith(synchronous);
  throws = false; access(); controller.dispose(); reject(late); await mutationDelivery();
  expect(controller.report).toHaveBeenCalledTimes(2); expect(controller.report).toHaveBeenLastCalledWith(late);
});

test("controller options and root admission reject malformed values with stable typed diagnostics", () => {
  const host = root(), report = vi.fn(), bindings = () => [];
  type Options = Parameters<typeof createHotkeyController>[1];
  const invalid = (value: unknown, message: string): void => uiError(() => createHotkeyController(host, value as Options), "value", message);
  uiError(() => createHotkeyController(document.createElement("div"), { bindings, report }), "usage", "Hotkey root must be a connected HTMLElement");
  invalid(null, "Hotkey controller options must be a plain object");
  invalid({ bindings, report, extra: true }, "Unknown hotkey controller option: extra");
  invalid({ bindings: [], report }, "Hotkey bindings must be a function");
  invalid({ bindings, report: null }, "Hotkey report must be a function");
  invalid({ bindings, report, blocked: true }, "Hotkey blocked must be a function");
});

test("binding admission rejects invalid records, callbacks, booleans, hints and foreign-document targets", () => {
  const host = root(), target = button(host), report = vi.fn();
  type Options = Parameters<typeof createHotkeyController>[1];
  const invalid = (bindings: unknown, message: string): void => uiError(() => createHotkeyController(host, { bindings: () => bindings, report } as unknown as Options), "value", message);
  invalid(null, "Hotkey bindings must be an array of at most 256 entries");
  invalid([null], "Hotkey binding must be a plain object");
  const base = { kind: "dom", chord: "alt+x", target };
  invalid([{ ...base, extra: true }], "Unknown hotkey binding option: extra");
  invalid([{ ...base, kind: "other" }], "Hotkey binding kind must be dom or callback");
  invalid([{ ...base, kind: "callback", run: "execute" }], "Hotkey callback must be a function");
  invalid([{ ...base, allowEditable: "yes" }], "Hotkey allowEditable must be a boolean");
  invalid([{ ...base, allowRepeat: 1 }], "Hotkey allowRepeat must be a boolean");
  invalid([{ ...base, hint: "x".repeat(33) }], "Hotkey hint must be text of at most 32 characters");
  invalid([{ ...base, hint: {} }], "Hotkey hint must be text of at most 32 characters");
  const foreign = document.implementation.createHTMLDocument().createElement("button");
  invalid([{ ...base, target: foreign }], "Hotkey target must be an HTMLElement in the controller document");
  invalid([{ ...base, target: {} }], "Hotkey target must be an HTMLElement in the controller document");
});

test("canonical chord admission rejects malformed and oversized declarations", () => {
  const host = root(), target = button(host), report = vi.fn();
  for (const chord of ["", "Alt+x", "alt+ctrl+x", "alt+alt+x", "alt+", "x+y", "alt+arrowleft", "x".repeat(33), 1]) {
    const declarations = [{ kind: "dom", chord, target }];
    uiError(() => createHotkeyController(host, { bindings: () => declarations, report } as unknown as Parameters<typeof createHotkeyController>[1]), "value", "Hotkey chord must be a canonical shortcut of at most 32 characters");
  }
});

test("binding and hint admission accepts exact limits and rejects limit plus one", () => {
  const host = root(), target = button(host), report = vi.fn();
  const full = Array.from({ length: 256 }, () => ({ kind: "dom" as const, chord: "alt+x", target, hint: "x".repeat(32) }));
  const controller = createHotkeyController(host, { bindings: () => full, report }); cleanups.push(controller.dispose);
  controller.refresh();
  uiError(() => createHotkeyController(host, { bindings: () => [...full, full[0]!], report }), "value", "Hotkey bindings must be an array of at most 256 entries");
});

test("document owner admission is bounded and released slots can be reused", () => {
  const host = root(), controllers = Array.from({ length: 128 }, () => owner(host, () => []));
  uiError(() => createHotkeyController(host, { bindings: () => [], report: vi.fn() }), "value", "Hotkey controller limit of 128 reached");
  controllers[0]!.dispose(); const replacement = owner(host, () => []); replacement.dispose();
});

test("boundary option admission, document limit and disposal are explicit", () => {
  const host = root(), report = vi.fn(); type Options = Parameters<typeof createHotkeyBoundary>[1];
  uiError(() => createHotkeyBoundary(host, null as unknown as Options), "value", "Hotkey boundary options must be a plain object");
  uiError(() => createHotkeyBoundary(host, { report, extra: true } as Options), "value", "Unknown hotkey boundary option: extra");
  uiError(() => createHotkeyBoundary(host, { report, onEscape: "close" } as unknown as Options), "value", "Hotkey onEscape must be a function");
  const boundaries = Array.from({ length: 32 }, () => boundary(root(host)));
  uiError(() => createHotkeyBoundary(root(host), { report }), "value", "Hotkey boundary limit of 32 reached");
  boundaries[0]!.dispose(); const replacement = boundary(root(host)); replacement.dispose(); replacement.dispose();
});

test("dynamic declaration and blocked predicate failures report to their owner without fallback", () => {
  const host = root(), target = button(host), fallback = vi.fn(); let broken = false;
  const first = owner(host, () => { if (broken) throw new Error("supplier failed"); return []; });
  owner(host, () => [{kind:"callback",chord:"alt+x",target,run:fallback}]);
  broken = true; expect(access().defaultPrevented).toBe(false); expect(fallback).not.toHaveBeenCalled();
  expect(first.report).toHaveBeenCalledWith(expect.objectContaining({message:"supplier failed"}));
  broken = false;
  const invalid = owner(host, () => [], () => "wrong" as unknown as boolean);
  access(); expect(invalid.report).toHaveBeenCalledWith(expect.objectContaining({code:"value",message:"Hotkey blocked must return a boolean"})); expect(fallback).not.toHaveBeenCalled();
});

test("Escape in an editor and boundary replacement act once, with explicit conflicts refused", () => {
  const host = root(), input = document.createElement("input"); host.append(input);
  let next: {dispose():void} | undefined;
  let current: {dispose():void}; const dismiss = vi.fn(() => { current.dispose(); next = boundary(root()); });
  current = boundary(host, dismiss); key(input, {key:"Escape",code:"Escape"}); expect(dismiss).toHaveBeenCalledOnce(); next?.dispose();
  const scope = boundary(host, dismiss);
  const commands = owner(host, () => [{kind:"callback",chord:"escape",target:input,run:vi.fn(),allowEditable:true}]);
  key(input,{key:"Escape",code:"Escape"});expect(dismiss).toHaveBeenCalledOnce();
  expect(commands.report).toHaveBeenCalledWith(expect.objectContaining({code:"value",message:"Duplicate active hotkey: escape."}));scope.dispose();
});


test("failed refresh clears hints and failed admission retains no stale command", () => {
  const host = root(), target = button(host); let broken = false;
  const current = owner(host, () => { if (broken) throw new Error("changed supplier"); return [{kind:"dom",chord:"alt+x",target}]; });
  alt(); expect(hints()).toHaveLength(1); broken = true;
  expect(() => current.refresh()).toThrowError("changed supplier"); expect(hints()).toHaveLength(0);
  expect(access().defaultPrevented).toBe(false); expect(current.report).toHaveBeenCalledWith(expect.objectContaining({message:"changed supplier"}));
});

test("boundary errors and rejected error reporters preserve failures in the browser channel", async () => {
  const host = root(), original = new Error("dismiss failed"), reporterFailure = new Error("report failed"), browserReport = vi.fn();
  vi.stubGlobal("reportError", browserReport);
  const modal = createHotkeyBoundary(host, {onEscape:()=>{throw original;},report:()=>Promise.reject(reporterFailure)}); cleanups.push(modal.dispose);
  key(host,{key:"Escape",code:"Escape"}); await mutationDelivery();
  expect(browserReport).toHaveBeenCalledOnce(); const aggregate=browserReport.mock.calls[0]![0] as AggregateError;
  expect(aggregate).toBeInstanceOf(AggregateError); expect(aggregate.errors).toEqual([original,reporterFailure]);
  vi.unstubAllGlobals();
});

test("DOM activation fails loudly when a target cannot receive focus", () => {
  const host=root(), target=root(host), click=vi.fn();target.addEventListener("click",click);
  const current=owner(host,()=>[{kind:"dom",chord:"alt+x",target}]);access();
  expect(click).not.toHaveBeenCalled();
  expect(current.report).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({code:"value",message:"Hotkey target could not receive focus"}));
});

for (const transition of ["dispose", "blocked", "replace", "editable-policy"] as const) {
  test(`DOM focus cannot activate after its owner becomes ${transition}`, () => {
    const host=root(), target=button(host), click=vi.fn();let blocked=false, replaced=false, allowEditable=true;
    const current=owner(host,()=>replaced?[]:[{kind:"dom",chord:"alt+x",target,allowEditable}],()=>blocked);
    target.addEventListener("click",click);
    target.addEventListener("focus",()=>{if(transition==="dispose")current.dispose();if(transition==="blocked")blocked=true;if(transition==="replace")replaced=true;if(transition==="editable-policy")allowEditable=false;});
    const source=transition==="editable-policy"?document.createElement("input"):document.body;if(source!==document.body)host.append(source);
    access(source);expect(click).not.toHaveBeenCalled();
    expect(current.report).toHaveBeenCalledWith(expect.objectContaining({code:"value",message:"Hotkey owner became unavailable before activation"}));
  });
}

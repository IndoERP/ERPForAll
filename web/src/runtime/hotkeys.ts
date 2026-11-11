/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { UIError } from "./errors.js";

interface BindingOptions {
  readonly chord: string;
  /** Eligibility/overlay anchor; a callback does not move its focus. */
  readonly target: HTMLElement;
  readonly allowEditable?: boolean;
  readonly allowRepeat?: boolean;
  readonly hint?: string;
}
/** A DOM command reuses native activation; a callback owns its asynchronous outcome. */
export type HotkeyBinding = BindingOptions & (
  { readonly kind: "dom" } | { readonly kind: "callback"; readonly run: () => unknown }
);
/** One disposable view's dynamic commands. Re-evaluated on every key dispatch. */
export interface HotkeyControllerOptions {
  readonly bindings: () => readonly HotkeyBinding[];
  readonly blocked?: () => boolean;
  readonly report: (error: unknown) => void;
}
/** An exclusive overlay. An empty boundary still excludes commands behind it. */
export interface HotkeyBoundaryOptions {
  readonly report: (error: unknown) => void;
  readonly onEscape?: () => unknown;
}
interface Controller { root: HTMLElement; options: HotkeyControllerOptions }
interface Boundary { root: HTMLElement; options: HotkeyBoundaryOptions }
interface Candidate { binding: HotkeyBinding; controller?: Controller; report: (error: unknown) => void }
const dispatchers = new WeakMap<Document, Dispatcher>();
const MODIFIERS = ["ctrl", "alt", "meta", "shift"] as const;

function plain(value: unknown, label: string, keys: readonly string[]): void {
  if (value === null || typeof value !== "object" || ![Object.prototype, null].includes(Object.getPrototypeOf(value))) {
    throw new UIError(`${label} options must be a plain object`, "value");
  }
  for (const key of Reflect.ownKeys(value)) if (typeof key !== "string" || !keys.includes(key)) throw new UIError(`Unknown ${label.toLowerCase()} option: ${String(key)}`, "value");
}
function rootDocument(root: HTMLElement): Document {
  if (!root?.ownerDocument?.defaultView || !(root instanceof root.ownerDocument.defaultView.HTMLElement) || !root.isConnected) throw new UIError("Hotkey root must be a connected HTMLElement", "usage");
  return root.ownerDocument;
}
/** Internal shared eligibility; no cached visibility survives a caller's render. */
export function hotkeyAvailable(element: HTMLElement): boolean {
  if (!element.isConnected || element.matches(":disabled") || element.closest('[hidden],[inert],[aria-hidden="true"],[aria-disabled="true"]')) return false;
  for (let parent: HTMLElement | null = element; parent; parent = parent.parentElement) {
    if (parent.matches("details:not([open])") && !parent.querySelector(":scope > summary")?.contains(element)) return false;
    const style = element.ownerDocument.defaultView!.getComputedStyle(parent);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
  }
  return true;
}
function editable(target: EventTarget | null, doc: Document): boolean {
  if (!(target instanceof doc.defaultView!.Element)) return false;
  if (target.closest("input,textarea,select")) return true;
  const holder = target.closest("[contenteditable]");
  return holder !== null && holder.getAttribute("contenteditable")?.toLowerCase() !== "false";
}
function chord(value: unknown): asserts value is string {
  if (typeof value !== "string" || value.length > 32 || !/^(?:ctrl\+)?(?:alt\+)?(?:meta\+)?(?:shift\+)?(?:[a-z0-9]|escape|enter)$/.test(value)) {
    throw new UIError("Hotkey chord must be a canonical shortcut of at most 32 characters", "value");
  }
}
function bindings(controller: Controller): readonly HotkeyBinding[] {
  const input = controller.options.bindings();
  if (!Array.isArray(input) || input.length > 256) throw new UIError("Hotkey bindings must be an array of at most 256 entries", "value");
  for (const binding of input) {
    if (binding === null || typeof binding !== "object" || ![Object.prototype, null].includes(Object.getPrototypeOf(binding))) throw new UIError("Hotkey binding must be a plain object", "value");
    plain(binding, "Hotkey binding", ["kind", "chord", "target", "run", "allowEditable", "allowRepeat", "hint"]);
    if (binding.kind !== "dom" && binding.kind !== "callback") throw new UIError("Hotkey binding kind must be dom or callback", "value");
    if (binding.kind === "callback" && typeof binding.run !== "function") throw new UIError("Hotkey callback must be a function", "value");
    if (binding.kind === "dom" && "run" in binding) throw new UIError("DOM hotkey binding cannot define run", "value");
    chord(binding.chord);
    const doc = controller.root.ownerDocument;
    if (!(binding.target instanceof doc.defaultView!.HTMLElement) || binding.target.ownerDocument !== doc) throw new UIError("Hotkey target must be an HTMLElement in the controller document", "value");
    for (const option of ["allowEditable", "allowRepeat"] as const) if (binding[option] !== undefined && typeof binding[option] !== "boolean") throw new UIError(`Hotkey ${option} must be a boolean`, "value");
    if (binding.hint !== undefined && (typeof binding.hint !== "string" || binding.hint.length > 32)) throw new UIError("Hotkey hint must be text of at most 32 characters", "value");
  }
  return input;
}
function eventChord(event: KeyboardEvent): string | undefined {
  const named = event.key === "Escape" ? "escape" : event.key === "Enter" ? "enter" : undefined;
  const physical = /^Key([A-Z])$/.exec(event.code)?.[1]?.toLowerCase() ?? /^(?:Digit|Numpad)([0-9])$/.exec(event.code)?.[1];
  const key = named ?? physical ?? (event.code === "" && /^[a-z0-9]$/i.test(event.key) ? event.key.toLowerCase() : undefined);
  if (key === undefined) return undefined;
  return MODIFIERS.filter((modifier) => event[`${modifier}Key`]).map(modifier => `${modifier}+`).join("") + key;
}
/** Report errors from a primitive without a client error service, preserving the browser error channel. */
export function reportHotkeyError(error: unknown): void {
  if (typeof globalThis.reportError === "function") globalThis.reportError(error);
  else setTimeout(() => { throw error; }, 0);
}
function report(reporter: (error: unknown) => void, error: unknown): void {
  try { Promise.resolve(reporter(error)).catch(reportFailure => reportHotkeyError(new AggregateError([error, reportFailure], "Hotkey error reporting failed"))); } catch (reportFailure) { reportHotkeyError(new AggregateError([error, reportFailure], "Hotkey error reporting failed")); }
}
function invoke(run: () => unknown, reporter: (error: unknown) => void): void {
  try { Promise.resolve(run()).catch(error => report(reporter, error)); }
  catch (error) { report(reporter, error); }
}
class Dispatcher {
  readonly controllers = new Set<Controller>();
  readonly boundaries: Boundary[] = [];
  private readonly hints: HTMLElement[] = [];
  private composing = false;
  private observer: MutationObserver | undefined;
  constructor(private readonly doc: Document) {
    doc.defaultView!.addEventListener("keydown", this.key);
    doc.defaultView!.addEventListener("keyup", this.clear);
    doc.defaultView!.addEventListener("blur", this.blur);
    doc.addEventListener("visibilitychange", this.clear);
    doc.addEventListener("compositionstart", this.compositionStart, true);
    doc.addEventListener("compositionend", this.compositionEnd, true);
  }
  readonly clear = (): void => {
    this.observer?.disconnect(); this.observer = undefined;
    for (const hint of this.hints.splice(0)) hint.remove();
  };
  private readonly blur = (): void => { this.composing = false; this.clear(); };
  private readonly compositionStart = (): void => { this.composing = true; this.clear(); };
  private readonly compositionEnd = (): void => { this.composing = false; };
  private activeScope(): HTMLElement | undefined {
    // jsdom lacks the native top layer: its tests use open fixtures, separately from native browser proof.
    const native = this.doc.defaultView!.CSS?.supports("selector(:modal)") === true;
    const dialogs = [...this.doc.querySelectorAll<HTMLElement>("dialog[open]")].filter(dialog => !native || dialog.matches(":modal"));
    const focused = this.doc.activeElement?.closest<HTMLElement>("dialog[open]");
    // Native showModal owns focus/top-layer order; unlike DOM order this survives retained shells.
    const nativeModal = dialogs.find(dialog => dialog === focused && native);
    const registeredModal = [...this.boundaries].reverse().find(boundary => dialogs.includes(boundary.root))?.root;
    const rank = (dialog: HTMLElement): number => this.boundaries.findLastIndex(entry => entry.root === dialog);
    // Unavailable native modals still impose UA inertness. An unknown hidden modal fails closed;
    // a known newer child can outrank its explicitly inert/aria-hidden managed parent.
    const unavailable = dialogs.filter(dialog => !hotkeyAvailable(dialog)).sort((a,b) => rank(a) - rank(b)).at(-1);
    const preferred = nativeModal ?? registeredModal;
    const preferredOwns = preferred && (!unavailable || preferred === unavailable || rank(unavailable) >= 0 && rank(preferred) > rank(unavailable));
    const modal = (preferredOwns ? preferred : unavailable) ?? preferred ?? dialogs.at(-1);
    const boundary = [...this.boundaries].reverse().find(entry => hotkeyAvailable(entry.root) && (!modal || entry.root === modal || entry.root.closest("dialog[open]") === modal));
    return boundary?.root ?? modal;
  }
  private candidates(event: KeyboardEvent, active: HTMLElement | undefined): Candidate[] | undefined {
    const result: Candidate[] = [];
    for (const controller of this.controllers) {
      if (!hotkeyAvailable(controller.root)) continue;
      try {
      if (controller.options.blocked) {
        const blocked = controller.options.blocked();
        if (typeof blocked !== "boolean") throw new UIError("Hotkey blocked must return a boolean", "value");
        if (blocked) continue;
      }
      for (const binding of bindings(controller)) {
        if (!hotkeyAvailable(binding.target) || active && !active.contains(binding.target)) continue;
        if (event.repeat && binding.allowRepeat !== true || editable(event.target, this.doc) && binding.allowEditable !== true) continue;
        result.push({ binding: { ...binding }, controller, report: controller.options.report });
      }
      } catch (error) { report(controller.options.report, error); return undefined; }
    }
    return result;
  }
  private readonly key = (event: KeyboardEvent): void => {
    this.clear();
    if (event.defaultPrevented || event.isComposing || event.keyCode === 229 || this.composing || event.getModifierState("AltGraph")) return;
    const active = this.activeScope();
    const candidates = this.candidates(event, active);
    if (!candidates) return; // A malformed dynamic owner aborts dispatch; no background fallback.
    if (event.key === "Alt" && event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey && !event.repeat) {
      for (const { binding } of candidates) if (/^alt\+(?:shift\+)?[a-z0-9]$/.test(binding.chord)) {
        const hint = this.doc.createElement("span"); hint.className = "button-hotkey-hint"; hint.dataset["hotkeyHint"] = "";
        hint.setAttribute("aria-hidden", "true"); hint.textContent = binding.hint ?? binding.chord.slice(4); binding.target.append(hint); this.hints.push(hint);
      }
      if (this.hints.length) {
        this.observer = new this.doc.defaultView!.MutationObserver(this.clear);
        this.observer.observe(this.doc.documentElement, { subtree: true, childList: true, attributes: true });
      }
      return;
    }
    const pressed = eventChord(event); if (pressed === undefined) return;
    const matches = candidates.filter(candidate => candidate.binding.chord === pressed);
    const boundary = [...this.boundaries].reverse().find(entry => entry.root === active);
    if (pressed === "escape" && !event.repeat && boundary?.options.onEscape) {
      matches.push({ binding: { kind: "callback", chord: "escape", target: boundary.root, run: boundary.options.onEscape, allowEditable: true }, report: boundary.options.report });
    }
    if (matches.length > 1) {
      event.preventDefault(); report(matches[0]!.report, new UIError(`Duplicate active hotkey: ${pressed}.`, "value")); return;
    }
    const candidate = matches[0]; if (!candidate) return;
    event.preventDefault();
    const binding = candidate.binding;
    invoke(() => {
      if (binding.kind === "callback") return binding.run();
      binding.target.focus();
      if (this.doc.activeElement !== binding.target) throw new UIError("Hotkey target could not receive focus", "value");
      const controller = candidate.controller;
      if (!controller || !this.controllers.has(controller) || !hotkeyAvailable(controller.root)) throw new UIError("Hotkey owner became unavailable before activation", "value");
      if (controller.options.blocked) {
        const blocked = controller.options.blocked();
        if (typeof blocked !== "boolean") throw new UIError("Hotkey blocked must return a boolean", "value");
        if (blocked) throw new UIError("Hotkey owner became unavailable before activation", "value");
      }
      if (!bindings(controller).some(current => current.kind === "dom" && current.target === binding.target && current.chord === binding.chord && (!event.repeat || current.allowRepeat === true) && (!editable(event.target, this.doc) || current.allowEditable === true))) throw new UIError("Hotkey owner became unavailable before activation", "value");
      if (!hotkeyAvailable(binding.target) || this.activeScope() !== active) throw new UIError("Hotkey target became unavailable before activation", "value");
      binding.target.click(); return undefined;
    }, candidate.report);
  };
  release(): void {
    this.clear(); if (this.controllers.size || this.boundaries.length) return;
    const view = this.doc.defaultView!;
    view.removeEventListener("keydown", this.key); view.removeEventListener("keyup", this.clear); view.removeEventListener("blur", this.blur);
    this.doc.removeEventListener("visibilitychange", this.clear); this.doc.removeEventListener("compositionstart", this.compositionStart, true); this.doc.removeEventListener("compositionend", this.compositionEnd, true);
    dispatchers.delete(this.doc);
  }
}
function dispatcher(doc: Document): Dispatcher {
  let value = dispatchers.get(doc);
  if (!value) { value = new Dispatcher(doc); dispatchers.set(doc, value); }
  return value;
}
/** Register a view's dynamic shortcuts. Throws UIError on malformed declarations/admission limits. Dispose on unmount. */
export function createHotkeyController(root: HTMLElement, options: HotkeyControllerOptions): { refresh(): void; dispose(): void } {
  const doc = rootDocument(root);
  plain(options, "Hotkey controller", ["bindings", "blocked", "report"]);
  if (typeof options.bindings !== "function") throw new UIError("Hotkey bindings must be a function", "value");
  if (typeof options.report !== "function") throw new UIError("Hotkey report must be a function", "value");
  if (options.blocked !== undefined && typeof options.blocked !== "function") throw new UIError("Hotkey blocked must be a function", "value");
  const controller = { root, options }; bindings(controller);
  const shared = dispatcher(doc);
  if (shared.controllers.size >= 128) throw new UIError("Hotkey controller limit of 128 reached", "value");
  shared.controllers.add(controller); shared.clear(); let disposed = false;
  return { refresh() { if (disposed) throw new UIError("Hotkey controller is disposed", "usage"); shared.clear(); bindings(controller); }, dispose() { if (disposed) return; disposed = true; shared.controllers.delete(controller); shared.release(); } };
}
/** Register an exclusive overlay after mounting. Newest eligible scope wins; native child modals outrank their parents. */
export function createHotkeyBoundary(root: HTMLElement, options: HotkeyBoundaryOptions): { dispose(): void } {
  const doc = rootDocument(root);
  plain(options, "Hotkey boundary", ["onEscape", "report"]);
  if (typeof options.report !== "function") throw new UIError("Hotkey report must be a function", "value");
  if (options.onEscape !== undefined && typeof options.onEscape !== "function") throw new UIError("Hotkey onEscape must be a function", "value");
  const shared = dispatcher(doc);
  if (shared.boundaries.length >= 32) throw new UIError("Hotkey boundary limit of 32 reached", "value");
  const boundary = { root, options }; shared.boundaries.push(boundary); shared.clear(); let disposed = false;
  return { dispose() { if (disposed) return; disposed = true; shared.boundaries.splice(shared.boundaries.indexOf(boundary), 1); shared.release(); } };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { UIError } from "./errors.js";
import { createHotkeyBoundary, reportHotkeyError } from "./hotkeys.js";

function connected(root: HTMLElement): void {
  if (!root.isConnected) throw new UIError("Accessibility root must be connected to its document", "usage");
}
function available(element: HTMLElement): boolean {
  if (!element.isConnected || element.closest('[hidden], [inert], [aria-hidden="true"]') || element.matches(":disabled, [aria-disabled='true']")) return false;
  for (let p: HTMLElement | null = element; p; p = p.parentElement) {
    if (p.matches("details:not([open])") && !p.querySelector(":scope > summary")?.contains(element)) return false;
    const style = p.ownerDocument.defaultView!.getComputedStyle(p);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
  }
  return true;
}
function restoreAttribute(element: HTMLElement, name: string, value: string | null): void {
  if (value === null) element.removeAttribute(name); else element.setAttribute(name, value);
}
interface TrapEntry { enter(): void }
const traps = new WeakMap<Document, TrapEntry[]>();
const FOCUSABLE = 'a[href], area[href], button, input:not([type="hidden"]), select, textarea, [tabindex], [contenteditable="true"], audio[controls], video[controls], summary';
/** Options for a modal focus controller. The dialog owns dismissal and background inertness. */
export interface FocusTrapOptions {
  readonly initialFocus?: HTMLElement;
  readonly onEscape?: () => void;
}
/** Trap keyboard/programmatic focus in a connected modal. Dispose on close; nesting uses last-opened priority. */
export function createFocusTrap(root: HTMLElement, options: FocusTrapOptions = {}): { dispose(): void } {
  connected(root);
  if (!available(root)) throw new UIError("Focus trap root must be available", "value");
  if (options.onEscape !== undefined && typeof options.onEscape !== "function") throw new UIError("Focus trap onEscape must be a function", "value");
  if (options.initialFocus && (!root.contains(options.initialFocus) || !available(options.initialFocus))) {
    throw new UIError("Focus trap initialFocus must be an available descendant", "value");
  }
  const doc = root.ownerDocument, prior = doc.activeElement;
  const tabindex = root.getAttribute("tabindex");
  root.tabIndex = -1;
  const stack = traps.get(doc) ?? []; traps.set(doc, stack);
  const token: TrapEntry = { enter: () => enter() }; stack.push(token); let disposed = false;
  const active = (): boolean => !disposed && stack.at(-1) === token;
  const candidates = (): HTMLElement[] => {
    const items = [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(e=>available(e) && e.tabIndex >= 0);
    return items.filter(element => {
      if (!element.matches('input[type="radio"]')) return true;
      const radio = element as HTMLInputElement;
      if (radio.name === "") return true;
      const group = items.filter((e): e is HTMLInputElement => e.matches('input[type="radio"]') &&
        (e as HTMLInputElement).name === radio.name && (e as HTMLInputElement).form === radio.form);
      return (group.find(e=>e.checked) ?? group[0]) === radio;
    }).sort((a,b)=>(a.tabIndex > 0 ? a.tabIndex : Infinity) - (b.tabIndex > 0 ? b.tabIndex : Infinity));
  };
  const enter = (): void => (candidates()[0] ?? root).focus();
  const focus = (event: FocusEvent): void => {
    if (active() && !root.contains(event.target as Node)) enter();
  };
  const key = (event: KeyboardEvent): void => {
    if (!active() || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key !== "Tab") return;
    const items = candidates(), index = items.indexOf(doc.activeElement as HTMLElement);
    event.preventDefault();
    if (!items.length) root.focus();
    else {
      const next = index < 0 ? (event.shiftKey ? items.length - 1 : 0)
        : (index + (event.shiftKey ? -1 : 1) + items.length) % items.length;
      items[next]!.focus();
    }
  };
  let hotkeys: ReturnType<typeof createHotkeyBoundary> | undefined;
  const dispose = (restoreFocus: boolean): void => {
    if (disposed) return;
    const wasActive = active(); disposed = true;
    stack.splice(stack.indexOf(token),1); hotkeys?.dispose();
    doc.removeEventListener("focusin",focus); doc.removeEventListener("keydown",key);
    restoreAttribute(root,"tabindex",tabindex);
    if (wasActive && restoreFocus) {
      if (prior instanceof doc.defaultView!.HTMLElement && available(prior)) prior.focus();
      else stack.at(-1)?.enter();
    }
  };
  doc.addEventListener("focusin", focus); doc.addEventListener("keydown", key);
  const initial = options.initialFocus ?? candidates()[0] ?? root;
  initial.focus();
  if (doc.activeElement !== initial) {
    dispose(false);
    throw new UIError("Focus trap initial target could not receive focus", "value");
  }
  try { hotkeys = createHotkeyBoundary(root, { report: reportHotkeyError, ...(options.onEscape ? { onEscape: options.onEscape } : {}) }); }
  catch (error) { dispose(false); throw error; }
  return { dispose() { dispose(true); } };
}

/** Roving collection ownership. Call refresh after replacing, hiding or disabling items. */
export interface RovingOptions {
  readonly items: () => readonly HTMLElement[];
  readonly orientation?: "horizontal" | "vertical" | "both";
  readonly wrap?: boolean;
}
/** One tab stop per collection; native controls retain their Enter/Space activation semantics. */
export function createRovingTabindex(root: HTMLElement, options: RovingOptions): { refresh(): void; dispose(): void } {
  connected(root);
  const originals = new Map<HTMLElement,string|null>(); let current: HTMLElement | undefined, disposed = false;
  const orientation = options.orientation ?? "both";
  if (!["horizontal","vertical","both"].includes(orientation)) throw new UIError("Invalid roving orientation", "value");
  const refresh = (): HTMLElement[] => {
    if (disposed) throw new UIError("Roving tabindex controller is disposed", "usage");
    const items = [...options.items()];
    if (new Set(items).size !== items.length || items.some(e=>!root.contains(e) || e === root)) {
      throw new UIError("Roving items must be unique descendants of the root", "value");
    }
    for (const [e,value] of originals) if (!items.includes(e)) { restoreAttribute(e,"tabindex",value); originals.delete(e); }
    const enabled = items.filter(available);
    if (!current || !enabled.includes(current)) current = enabled[0];
    for (const item of items) {
      if (!originals.has(item)) originals.set(item,item.getAttribute("tabindex"));
      item.tabIndex = item === current ? 0 : -1;
    }
    return enabled;
  };
  const focus = (event: FocusEvent): void => {
    const items = refresh(), item = items.find(e=>e===event.target);
    if (item) { current = item; refresh(); }
  };
  const key = (event: KeyboardEvent): void => {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const items = refresh(), index = items.indexOf(event.target as HTMLElement);
    if (index < 0) return;
    let next: number;
    if (event.key === "Home") next = 0;
    else if (event.key === "End") next = items.length - 1;
    else {
      const delta = ((orientation !== "vertical" && event.key === "ArrowRight") || (orientation !== "horizontal" && event.key === "ArrowDown")) ? 1
        : ((orientation !== "vertical" && event.key === "ArrowLeft") || (orientation !== "horizontal" && event.key === "ArrowUp")) ? -1 : 0;
      if (delta === 0) return;
      next = index + delta;
      if (options.wrap !== false) next = (next + items.length) % items.length;
      else next = Math.max(0,Math.min(items.length-1,next));
    }
    event.preventDefault(); current = items[next]; refresh(); current!.focus();
  };
  refresh(); root.addEventListener("focusin",focus); root.addEventListener("keydown",key);
  return { refresh() { refresh(); }, dispose() {
    if (disposed) return; disposed = true;
    root.removeEventListener("focusin",focus); root.removeEventListener("keydown",key);
    for (const [element,value] of originals) restoreAttribute(element,"tabindex",value);
    originals.clear(); current = undefined;
  } };
}

/** Session-local announcement service. Own one per mounted app and destroy with the app. */
export class LiveRegion {
  /** Register this canonical class with ServiceContainer; app teardown still owns destroy(). */
  static start(): LiveRegion { return new LiveRegion(document.body); }
  private readonly regions: Record<"polite"|"assertive", HTMLElement>;
  private readonly timers = new Map<HTMLElement,ReturnType<typeof setTimeout>>();
  private destroyed = false;
  constructor(root: HTMLElement) {
    connected(root);
    const make = (urgency: "polite"|"assertive"): HTMLElement => {
      const node = root.ownerDocument.createElement("div");
      node.setAttribute("role",urgency === "polite" ? "status" : "alert");
      node.setAttribute("aria-live",urgency); node.setAttribute("aria-atomic","true");
      node.style.cssText = "position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap;border:0";
      root.append(node); return node;
    };
    this.regions = { polite: make("polite"), assertive: make("assertive") };
  }
  /** Latest message per urgency wins. Plain text only, bounded to 4096 UTF-16 units. */
  announce(message: string, urgency: "polite"|"assertive" = "polite"): void {
    if (this.destroyed) throw new UIError("Live region is destroyed", "usage");
    if (typeof message !== "string" || message.length > 4096) throw new UIError("Live region message must be a string of at most 4096 characters", "value");
    if (urgency !== "polite" && urgency !== "assertive") throw new UIError("Invalid live region urgency", "value");
    const node = this.regions[urgency], pending = this.timers.get(node);
    if (pending !== undefined) clearTimeout(pending);
    node.textContent = "";
    this.timers.set(node,setTimeout(()=>{ this.timers.delete(node); node.textContent = message; }, 50));
  }
  /** Cancel pending announcements and release both DOM regions. Idempotent. */
  destroy(): void {
    if (this.destroyed) return; this.destroyed = true;
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear(); for (const node of Object.values(this.regions)) node.remove();
  }
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFocusTrap, createRovingTabindex, LiveRegion, createEnv } from "../src/index.js";

import { uiError } from "./error-contract.js";

const cleanups: (() => void)[] = [];
afterEach(() => { for (const dispose of cleanups.splice(0).reverse()) dispose(); document.body.replaceChildren(); vi.useRealTimers(); });
function button(parent: HTMLElement, label: string): HTMLButtonElement {
  const b = document.createElement("button"); b.textContent = label; parent.append(b); return b;
}
function root(): HTMLDivElement { const r = document.createElement("div"); document.body.append(r); return r; }
function key(target: Element, key: string, shiftKey = false): KeyboardEvent {
  const e = new KeyboardEvent("keydown", { key, shiftKey, bubbles: true, cancelable: true }); target.dispatchEvent(e); return e;
}
/// (partial: reusable modal focus and disposal, no hotkeys/compiler)
test("focus trap wraps, contains outside focus and restores the trigger", () => {
  const trigger = button(document.body, "open"); trigger.focus(); const r = root();
  const a = button(r, "first"), b = button(r, "last"), hidden = button(r, "hidden"); hidden.hidden = true;
  const escape = vi.fn(); const trap = createFocusTrap(r, { onEscape: escape }); cleanups.push(trap.dispose);
  expect(document.activeElement).toBe(a); b.focus(); expect(key(b, "Tab").defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(a); key(a, "Tab", true); expect(document.activeElement).toBe(b);
  trigger.focus(); expect(document.activeElement).toBe(a); key(a, "Escape"); expect(escape).toHaveBeenCalledOnce();
  trap.dispose(); expect(document.activeElement).toBe(trigger); trap.dispose();
});
test("nested traps suspend the outer trap and restore in stack order", () => {
  const outer = root(), a = button(outer, "outer"); const first = createFocusTrap(outer); cleanups.push(first.dispose);
  const inner = root(), b = button(inner, "inner"); const second = createFocusTrap(inner); cleanups.push(second.dispose);
  a.focus(); expect(document.activeElement).toBe(b); second.dispose(); expect(document.activeElement).toBe(a);
});
test("empty trap receives focus, restores tabindex and rejects disconnected roots", () => {
  const r = root(); const trap = createFocusTrap(r); cleanups.push(trap.dispose);
  expect(document.activeElement).toBe(r); trap.dispose(); expect(r.hasAttribute("tabindex")).toBe(false);
  uiError(() => createFocusTrap(document.createElement("div")), "usage", "Accessibility root must be connected to its document");
});
/// (partial: roving tabindex primitives, no hotkeys/compiler)
test("roving skips disabled/hidden items, respects orientation and restores attributes", () => {
  const r = root(), a = button(r, "a"), b = button(r, "b"), c = button(r, "c"); b.disabled = true;
  a.setAttribute("tabindex", "3"); const roving = createRovingTabindex(r, { items: () => [a,b,c], orientation: "horizontal" }); cleanups.push(roving.dispose);
  expect([a.tabIndex,b.tabIndex,c.tabIndex]).toEqual([0,-1,-1]); a.focus();
  expect(key(a,"ArrowDown").defaultPrevented).toBe(false); key(a,"ArrowRight"); expect(document.activeElement).toBe(c);
  key(c,"ArrowRight"); expect(document.activeElement).toBe(a); key(a,"End"); expect(document.activeElement).toBe(c);
  c.hidden = true; roving.refresh(); expect(a.tabIndex).toBe(0); expect(c.tabIndex).toBe(-1);
  roving.dispose(); expect(a.getAttribute("tabindex")).toBe("3"); expect(c.hasAttribute("tabindex")).toBe(false);
});
test("roving follows pointer focus and refreshes replacement items", () => {
  const r = root(), a = button(r,"a"), b = button(r,"b"); let items = [a,b];
  const roving = createRovingTabindex(r,{items:()=>items}); cleanups.push(roving.dispose);
  b.focus(); expect(a.tabIndex).toBe(-1); expect(b.tabIndex).toBe(0);
  b.remove(); const c = button(r,"c"); items = [a,c]; roving.refresh(); expect(a.tabIndex).toBe(0);
  key(a,"ArrowDown"); expect(document.activeElement).toBe(c);
});
test("live regions repeat text, isolate urgency and cancel pending delivery on destroy", () => {
  vi.useFakeTimers(); const r = root(), live = new LiveRegion(r); cleanups.push(()=>live.destroy());
  live.announce("Saved"); vi.runAllTimers(); expect(r.querySelector('[role="status"]')?.textContent).toBe("Saved");
  live.announce("Saved"); expect(r.querySelector('[role="status"]')?.textContent).toBe(""); vi.runAllTimers();
  live.announce("Failed", "assertive"); vi.runAllTimers(); expect(r.querySelector('[role="alert"]')?.textContent).toBe("Failed");
  live.announce("late"); live.destroy(); vi.runAllTimers(); expect(r.children).toHaveLength(0);
  uiError(()=>live.announce("late"), "usage", "Live region is destroyed");
});
test("live regions treat content as text and reject oversized messages", () => {
  vi.useFakeTimers(); const r = root(), live = new LiveRegion(r); cleanups.push(()=>live.destroy());
  live.announce("<img src=x>"); vi.runAllTimers(); expect(r.querySelector("img")).toBeNull();
  uiError(()=>live.announce("x".repeat(4097)), "value", "Live region message must be a string of at most 4096 characters");
});
test("trap follows positive tabindex order and radio group tab stops", () => {
  const r = root(), a = button(r,"a"), b = button(r,"b"), c = button(r,"c"); a.tabIndex=2; b.tabIndex=1;
  const radio = (checked: boolean): HTMLInputElement => { const e=document.createElement("input"); e.type="radio"; e.name="group"; e.checked=checked; r.append(e); return e; };
  radio(false); const selected=radio(true); const trap=createFocusTrap(r); cleanups.push(trap.dispose);
  expect(document.activeElement).toBe(b); key(b,"Tab"); expect(document.activeElement).toBe(a);
  key(a,"Tab"); expect(document.activeElement).toBe(c); key(c,"Tab"); expect(document.activeElement).toBe(selected);
  key(selected,"Tab"); expect(document.activeElement).toBe(b); key(b,"Tab",true); expect(document.activeElement).toBe(selected);
});
test("failed trap entry rolls back and unavailable roots fail", () => {
  const r=root(), target=document.createElement("div"), outside=button(document.body,"outside"); r.append(target); outside.focus();
  uiError(()=>createFocusTrap(r,{initialFocus:target}),"value","Focus trap initial target could not receive focus");
  expect(r.hasAttribute("tabindex")).toBe(false); outside.focus(); expect(document.activeElement).toBe(outside);
  target.tabIndex=-1; const trap=createFocusTrap(r,{initialFocus:target}); expect(document.activeElement).toBe(target); trap.dispose();
  r.hidden=true; uiError(()=>createFocusTrap(r),"value","Focus trap root must be available");
});
test("removed nested trigger falls back to the outer trap", () => {
  const outer=root(), a=button(outer,"a"), b=button(outer,"b"); const first=createFocusTrap(outer); cleanups.push(first.dispose); b.focus();
  const inner=root(); button(inner,"inner"); const second=createFocusTrap(inner); cleanups.push(second.dispose); b.remove(); second.dispose();
  expect(document.activeElement).toBe(a);
});
test("roving error kinds and stable messages", () => {
  const r=root(), a=button(r,"a"), outside=button(document.body,"outside");
  uiError(()=>createRovingTabindex(r,{items:()=>[a],orientation:"diagonal" as "both"}),"value","Invalid roving orientation");
  for (const items of [[a,a],[outside]]) uiError(()=>createRovingTabindex(r,{items:()=>items}),"value","Roving items must be unique descendants of the root");
  const roving=createRovingTabindex(r,{items:()=>[a],wrap:false}); a.focus(); key(a,"ArrowDown"); expect(document.activeElement).toBe(a);
  roving.dispose(); uiError(()=>roving.refresh(),"usage","Roving tabindex controller is disposed");
});
test("canonical live service can be registered and invalid urgency fails", async () => {
  const env=createEnv(); env.services.add(LiveRegion); await env.services.start(env);
  const service=env.services.get(LiveRegion); cleanups.push(()=>service.destroy());
  uiError(()=>service.announce("test","loud" as "polite"),"value","Invalid live region urgency");
});

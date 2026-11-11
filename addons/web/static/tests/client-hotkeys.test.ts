/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { ButtonHotkeys } from "../src/client/view-button.js";
const stops: (() => void)[] = [];
afterEach(() => { for (const stop of stops.splice(0).reverse()) stop(); document.body.replaceChildren(); });
function owner() {
  const control = document.createElement("button"), call = vi.fn(), report = vi.fn();
  control.dataset["hotkey"] = "x"; control.addEventListener("click", call); document.body.append(control);
  const keys = new ButtonHotkeys(() => [control], { blocked: () => false, report }); stops.push(() => keys.dispose());
  return { control, call, report };
}
function key(target: EventTarget, init: KeyboardEventInit = {}) {
  const event = new KeyboardEvent("keydown", { key: "x", code: "KeyX", altKey: true, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event); return event;
}
/// (partial: shared conflict admission across two view owners)
test("two eligible owners report a conflict and execute neither button", () => {
  const first = owner(), second = owner(); const event = key(document);
  expect(first.call).not.toHaveBeenCalled(); expect(second.call).not.toHaveBeenCalled();
  expect(first.report.mock.calls.length + second.report.mock.calls.length).toBe(1);
  expect(event.defaultPrevented).toBe(true);
});
/// (partial: consumed events and IME admission)
test("declared buttons preserve consumed events and composing text", () => {
  const current = owner(); key(current.control, { isComposing: true });
  current.control.addEventListener("keydown", event => event.preventDefault()); key(current.control);
  expect(current.call).not.toHaveBeenCalled(); expect(current.report).not.toHaveBeenCalled();
});
/// (partial: inherited editable protection and aria availability)
test("inherited editable and aria-disabled controls cannot activate", () => {
  const current = owner(); current.control.setAttribute("aria-disabled", "true"); key(document);
  current.control.removeAttribute("aria-disabled");
  const editor = document.createElement("div"), child = document.createElement("span"); editor.setAttribute("contenteditable", "plaintext-only"); editor.append(child); document.body.append(editor); key(child);
  expect(current.call).not.toHaveBeenCalled();
});

test("a background search menu cannot consume a newer boundary Escape or composition session",async()=>{
  const {createSearchMenuPanel}=await import("../src/client/search/search-menu-panel.js");
  const {SearchModel}=await import("../src/client/search/search-model.js");
  const {createHotkeyBoundary}=await import("@erpforall/elang");
  const trigger=document.createElement("button");document.body.append(trigger);
  const menu=createSearchMenuPanel(new SearchModel({model:"test.note",fields:new Map(),userId:2,companyId:1}),()=>{},trigger);document.body.append(menu.element);menu.open();stops.push(menu.dispose);
  const newer=document.createElement("div");newer.tabIndex=-1;document.body.append(newer);const dismiss=vi.fn();const boundary=createHotkeyBoundary(newer,{onEscape:dismiss,report:error=>{throw error;}});stops.push(boundary.dispose);
  key(newer,{key:"Escape",code:"Escape",altKey:false});expect(menu.isOpen()).toBe(true);expect(dismiss).toHaveBeenCalledOnce();boundary.dispose();
  menu.element.dispatchEvent(new CompositionEvent("compositionstart",{bubbles:true}));key(menu.element,{key:"Escape",code:"Escape",altKey:false});expect(menu.isOpen()).toBe(true);
  menu.element.dispatchEvent(new CompositionEvent("compositionend",{bubbles:true}));key(menu.element,{key:"Escape",code:"Escape",altKey:false});expect(menu.isOpen()).toBe(false);
});

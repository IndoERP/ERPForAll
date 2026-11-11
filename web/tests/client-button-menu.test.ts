/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { withTestScope } from "@erpforall/elang-test";
import { auditComponent } from "../testing/axe.js";
import { createButtonMenu, type ButtonMenuController } from "../../addons/web/static/src/client/button-menu.js";
import { ClientError } from "../../addons/web/static/src/client/transport.js";

const disposals: (() => void)[] = [];
afterEach(() => { disposals.splice(0).reverse().forEach(dispose => dispose()); document.body.replaceChildren(); vi.restoreAllMocks(); });
function control(parent: HTMLElement, label: string): HTMLButtonElement { const node = document.createElement("button"); node.type = "button"; node.textContent = label; parent.append(node); return node; }
function key(target: HTMLElement, value: string, shiftKey = false): KeyboardEvent {
  const event = new KeyboardEvent("keydown", { key: value, shiftKey, bubbles: true, cancelable: true }); target.dispatchEvent(event); return event;
}
function fixture(): { host: HTMLElement; opener: HTMLButtonElement; menu: HTMLElement; items: HTMLButtonElement[]; controller: ButtonMenuController } {
  const host = document.createElement("div"); document.body.append(host); const opener = control(host, "More");
  const menu = document.createElement("div"); host.append(menu); const items = [control(menu, "First"), control(menu, "Disabled"), control(menu, "Last"), control(menu, "Hidden")];
  items[1]!.disabled = true; items[3]!.hidden = true;
  const controller = createButtonMenu({ opener, menu, items: () => items }); disposals.push(controller.dispose);
  return { host, opener, menu, items, controller };
}

test("dropdown traversal skips unavailable items and selection executes the original control once", async () => withTestScope(async () => {
  const { opener, menu, items, controller } = fixture(); const action = vi.fn(); items[2]!.addEventListener("click", action);
  expect(menu.hidden).toBe(true); expect(opener.getAttribute("aria-expanded")).toBe("false");
  opener.disabled = true; opener.click(); controller.open(); expect(menu.hidden).toBe(true); opener.disabled = false;
  key(opener, "ArrowDown"); expect(menu.hidden).toBe(false); expect(document.activeElement).toBe(items[0]);
  expect(opener.getAttribute("aria-controls")).toBe(menu.id); expect(opener.getAttribute("aria-expanded")).toBe("true");
  key(items[0]!, "ArrowDown"); expect(document.activeElement).toBe(items[2]);
  key(items[2]!, "ArrowDown"); expect(document.activeElement).toBe(items[0]);
  key(items[0]!, "End"); expect(document.activeElement).toBe(items[2]);
  key(items[2]!, "Home"); expect(document.activeElement).toBe(items[0]);
  key(items[0]!, "Tab", true); expect(document.activeElement).toBe(items[2]);
  key(items[2]!, "Tab"); expect(document.activeElement).toBe(items[0]);
  key(items[0]!, "ArrowUp"); key(items[2]!, "Enter");
  expect(action).toHaveBeenCalledOnce(); expect(menu.hidden).toBe(true); expect(document.activeElement).toBe(opener);
  key(opener, "ArrowUp"); expect(document.activeElement).toBe(items[2]); key(items[2]!, "Escape");
  expect(document.activeElement).toBe(opener); expect(menu.hidden).toBe(true);
  nestedPolicies();
}));

test("outside dismissal preserves the new focus and disposal restores owned attributes", () => {
  const { opener, menu, items, controller } = fixture(); const outside = control(document.body, "Outside");
  opener.setAttribute("data-label", "unrelated"); controller.open(); outside.focus(); outside.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(menu.hidden).toBe(true); expect(document.activeElement).toBe(outside);
  controller.open(); controller.dispose(); controller.dispose();
  expect(opener.hasAttribute("aria-expanded")).toBe(false); expect(menu.hasAttribute("role")).toBe(false);
  expect(items[0]!.hasAttribute("role")).toBe(false); expect(items[0]!.hasAttribute("tabindex")).toBe(false);
  expect(opener.getAttribute("data-label")).toBe("unrelated");
  opener.click(); expect(menu.hidden).toBe(false);
});

test("empty menus stay closed and refresh removes an unavailable focused target", () => {
  const { opener, menu, items, controller } = fixture(); controller.open(); items[0]!.hidden = true; controller.refresh();
  expect(document.activeElement).toBe(items[2]); items[2]!.disabled = true; controller.refresh();
  expect(menu.hidden).toBe(true); expect(document.activeElement).toBe(opener);
});

test("ordinary mounted menu semantics pass shared axe audit", async () => withTestScope(async () => {
  const { host, controller } = fixture(); controller.open(); await auditComponent(host);
}));

function nestedPolicies(): void {
  const host = document.createElement("div"); document.body.append(host); const opener = control(host, "Outer");
  const outer = document.createElement("div"); host.append(outer); const childOpener = control(outer, "Child");
  const inner = document.createElement("div"); outer.append(inner); const item = control(inner, "Run");
  let mode: "none" | "closest" | "all" = "none"; const action = vi.fn(); item.addEventListener("click", action);
  const parent = createButtonMenu({ opener, menu: outer, items: () => [childOpener] }); disposals.push(parent.dispose);
  const child = createButtonMenu({ opener: childOpener, menu: inner, items: () => [item], closingMode: () => mode }); disposals.push(child.dispose);
  parent.open(); childOpener.click(); expect(outer.hidden).toBe(false); expect(inner.hidden).toBe(false);
  item.click(); expect(inner.hidden).toBe(false); expect(outer.hidden).toBe(false);
  mode = "closest"; item.click(); expect(inner.hidden).toBe(true); expect(outer.hidden).toBe(false); expect(document.activeElement).toBe(childOpener);
  child.open(); key(item, "Escape"); expect(inner.hidden).toBe(true); expect(outer.hidden).toBe(false);
  child.open(); mode = "all"; item.click(); expect(inner.hidden).toBe(true); expect(outer.hidden).toBe(true); expect(document.activeElement).toBe(opener);
  expect(action).toHaveBeenCalledTimes(3);
}

test("invalid ownership and closing mode fail with kind and message", () => {
  const { opener, menu, controller } = fixture(); controller.dispose();
  for (const [factory, message] of [
    [() => createButtonMenu({ opener, menu, items: () => [], closingMode: "unknown" as "all" }), "Unknown button menu closing mode: unknown."],
    [() => createButtonMenu({ opener, menu: document.implementation.createHTMLDocument().createElement("div"), items: () => [] }), "Button menu and opener must belong to the same document."]
  ] as const) {
    try { factory(); throw new Error("Expected refusal"); } catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message }); }
  }
});

test("disconnected construction is allowed but opening requires a connected owner", () => {
  const opener = document.createElement("button"), menu = document.createElement("div"); const item = control(menu, "Item");
  const controller = createButtonMenu({ opener, menu, items: () => [item] }); disposals.push(controller.dispose);
  try { controller.open(); throw new Error("Expected refusal"); } catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message: "Button menu and opener must be connected before opening." }); }
  document.body.append(opener, menu); controller.open(); expect(document.activeElement).toBe(item);
});

test("selection does not steal focus from a newly opened action surface", () => {
  const { menu, items, controller } = fixture(); const destination = document.createElement("input"); destination.setAttribute("aria-label", "Action dialog field"); document.body.append(destination);
  items[0]!.addEventListener("click", () => destination.focus()); controller.open(); items[0]!.click();
  expect(menu.hidden).toBe(true); expect(document.activeElement).toBe(destination);
});

test("an action becoming pending during its click still closes the menu", () => {
  const { menu, items, controller } = fixture(); items[0]!.addEventListener("click", () => { items[0]!.disabled = true; });
  controller.open(); items[0]!.click(); expect(menu.hidden).toBe(true); expect(items[0]!.disabled).toBe(true);
});

test("arrow activation from an already expanded opener enters its endpoint", () => {
  const { opener, items, controller } = fixture(); controller.open(); opener.focus(); key(opener, "ArrowUp");
  expect(document.activeElement).toBe(items[2]); opener.focus(); key(opener, "ArrowDown"); expect(document.activeElement).toBe(items[0]);
});

/// (partial: viewport-constrained dropdown placement and geometry lifecycle)
test("menus flip above a bottom-edge opener, clamp horizontally and scroll within the viewport", () => {
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(300); vi.spyOn(window, "innerHeight", "get").mockReturnValue(600);
  const { opener, menu, controller } = fixture();
  let openerBounds = new DOMRect(-20, 570, 40, 24);
  vi.spyOn(opener, "getBoundingClientRect").mockImplementation(() => openerBounds);
  vi.spyOn(menu, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 240, 200));
  controller.open();
  expect(menu.style.position).toBe("fixed");
  expect(Number.parseFloat(menu.style.top)).toBeGreaterThanOrEqual(8);
  expect(Number.parseFloat(menu.style.top) + Math.min(200, Number.parseFloat(menu.style.maxHeight))).toBeLessThanOrEqual(592);
  expect(Number.parseFloat(menu.style.top)).toBeLessThan(openerBounds.top);
  expect(Number.parseFloat(menu.style.left)).toBe(8); expect(menu.style.overflowY).toBe("auto");
  expect(Number.parseFloat(menu.style.maxWidth)).toBeLessThanOrEqual(284);
  openerBounds = new DOMRect(290, 570, 40, 24); window.dispatchEvent(new Event("resize"));
  expect(Number.parseFloat(menu.style.left)).toBeLessThanOrEqual(52);
});

test("menu geometry follows resize and scroll only while open and restores only owned style properties", () => {
  let viewportHeight = 600;
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(300); vi.spyOn(window, "innerHeight", "get").mockImplementation(() => viewportHeight);
  const { opener, menu, controller } = fixture(); let openerBounds = new DOMRect(180, 570, 100, 24);
  const measureOpener = vi.spyOn(opener, "getBoundingClientRect").mockImplementation(() => openerBounds);
  vi.spyOn(menu, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 240, 200));
  menu.style.position = "absolute"; menu.style.setProperty("top", "13px", "important"); menu.style.left = "17px";
  menu.style.maxHeight = "111px"; menu.style.overflowY = "visible"; menu.style.color = "red";
  controller.open(); menu.style.color = "blue";
  viewportHeight = 180; openerBounds = new DOMRect(180, 150, 100, 24); window.dispatchEvent(new Event("resize"));
  expect(Number.parseFloat(menu.style.top)).toBeGreaterThanOrEqual(8);
  expect(Number.parseFloat(menu.style.top) + Math.min(200, Number.parseFloat(menu.style.maxHeight))).toBeLessThanOrEqual(172);
  expect(Number.parseFloat(menu.style.maxHeight)).toBeLessThan(200);
  const oldTop = menu.style.top; openerBounds = new DOMRect(180, 20, 100, 24); document.dispatchEvent(new Event("scroll"));
  expect(menu.style.top).not.toBe(oldTop);
  controller.close(false);
  expect(menu.style.position).toBe("absolute"); expect(menu.style.top).toBe("13px"); expect(menu.style.getPropertyPriority("top")).toBe("important");
  expect(menu.style.left).toBe("17px"); expect(menu.style.maxHeight).toBe("111px"); expect(menu.style.overflowY).toBe("visible"); expect(menu.style.color).toBe("blue");
  measureOpener.mockClear(); window.dispatchEvent(new Event("resize")); document.dispatchEvent(new Event("scroll")); expect(measureOpener).not.toHaveBeenCalled();
  menu.style.top = "19px"; controller.open(); controller.dispose();
  expect(menu.style.top).toBe("19px"); expect(menu.style.color).toBe("blue");
  measureOpener.mockClear(); window.dispatchEvent(new Event("resize")); document.dispatchEvent(new Event("scroll")); expect(measureOpener).not.toHaveBeenCalled();
});

test("fixed menu placement compensates the existing translated dialog containing block", () => {
  vi.spyOn(window, "innerWidth", "get").mockReturnValue(400); vi.spyOn(window, "innerHeight", "get").mockReturnValue(600);
  const { host, opener, menu, controller } = fixture(); host.style.transform = "translate(-50%, -50%)";
  vi.spyOn(host, "getBoundingClientRect").mockReturnValue(new DOMRect(40, 100, 260, 380));
  vi.spyOn(host, "clientLeft", "get").mockReturnValue(2); vi.spyOn(host, "clientTop", "get").mockReturnValue(3);
  vi.spyOn(opener, "getBoundingClientRect").mockReturnValue(new DOMRect(180, 450, 100, 24));
  vi.spyOn(menu, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 200, 180));
  controller.open();
  expect(menu.style.position).toBe("fixed");
  expect(Number.parseFloat(menu.style.left) + 40 + 2).toBe(80);
  expect(Number.parseFloat(menu.style.top) + 100 + 3).toBe(266);
});

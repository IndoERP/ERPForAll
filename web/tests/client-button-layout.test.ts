/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { renderFormLayout } from "../../addons/web/static/src/client/form-layout.js";
import type { ButtonNode, Node } from "../../addons/web/static/src/client/metadata.js";

const disposals: (() => void)[] = [];
afterEach(() => { disposals.splice(0).forEach(stop => stop()); document.body.replaceChildren(); vi.restoreAllMocks(); });
const action = (name: string): ButtonNode => ({ kind: "button", name, label: name, variant: "secondary", confirm: null, invisible: null });
function mount(kind: "header" | "button_box", count: number, hidden: string[] = []) {
  const host = document.createElement("form"); document.body.append(host);
  const calls: string[] = [];
  const nodes: Node[] = [{ kind, children: Array.from({length: count}, (_, i) => kind === "header" ? action(`action_${i}`) : { ...action(`action_${i}`), kind: "stat_button", children: [] }) }];
  const layout = renderFormLayout(host, nodes, () => undefined, new Map(), (node, parent) => {
    if (hidden.includes(node.name)) return;
    const control = document.createElement("button"); control.type = "button";
    control.textContent = node.label; control.dataset["focusKey"] = node.name;
    control.addEventListener("click", () => calls.push(node.name)); parent.append(control); return control;
  });
  disposals.push(() => layout.dispose());
  return {host, calls};
}
function width(value: number) { vi.spyOn(window, "innerWidth", "get").mockReturnValue(value); window.dispatchEvent(new Event("resize")); }

/// (partial: visibility-filtered phone action layout; mounted record transitions covered separately)
///
test("phone header keeps the first visible action direct and the toggle never executes it", () => {
  width(390);
  const {host, calls} = mount("header", 4, ["action_0"]);
  const direct = host.querySelector(".form-header-actions")!;
  expect([...direct.querySelectorAll(":scope > button")].map(b => b.textContent)).toEqual(["action_1", "More actions"]);
  const more = direct.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!;
  more.click(); expect(calls).toEqual([]); expect(more.getAttribute("aria-expanded")).toBe("true");
  const menu = direct.querySelector<HTMLElement>('[role="menu"]')!;
  expect([...menu.querySelectorAll("button")].map(b => b.textContent)).toEqual(["action_2", "action_3"]);
  menu.querySelector<HTMLButtonElement>("button")!.click(); expect(calls).toEqual(["action_2"]);
  expect(more.getAttribute("aria-expanded")).toBe("false");
  width(1280);
  expect([...direct.querySelectorAll(":scope > button")].map(b => b.textContent)).toEqual(["action_1", "action_2", "action_3"]);
  expect(direct.querySelector('[aria-haspopup="menu"]')).toBeNull();
  width(390);
  expect(mount("header", 0).host.querySelector("button")).toBeNull();
  expect(mount("header", 1).host.querySelector('[aria-haspopup="menu"]')).toBeNull();
});

/// (partial: empty/single phone groups and direct desktop actions)
test("zero or one visible header action adds no toggle; desktop keeps every action direct", () => {
  width(390);
  expect(mount("header", 0).host.querySelector("button")).toBeNull();
  expect(mount("header", 1).host.querySelector('[aria-haspopup="menu"]')).toBeNull();
  width(1280);
  const {host} = mount("header", 4);
  expect(host.querySelector('[aria-haspopup="menu"]')).toBeNull();
  expect(host.querySelectorAll("button")).toHaveLength(4);
});

test("button box partitions in order, reserves More space, and preserves original controls through resize", () => {
  width(1280);
  const {host, calls} = mount("button_box", 9, ["action_0"]);
  const controls = [...host.querySelectorAll<HTMLButtonElement>('[data-focus-key]')];
  const more = host.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!;
  expect(more).not.toBeNull();
  const menu = host.querySelector<HTMLElement>('[role="menu"]')!;
  expect(menu.querySelectorAll("button")).toHaveLength(3);
  expect(host.querySelectorAll(".form-button-box > button")).toHaveLength(6);
  expect(mount("button_box", 6).host.querySelector('[aria-haspopup="menu"]')).toBeNull();
  expect(mount("button_box", 2, ["action_0", "action_1"]).host.querySelector("button")).toBeNull();
  expect([...host.querySelectorAll('[data-focus-key]')]).toEqual(controls);
  width(390);
  // Phone and tablet widths keep three statistics beside More; the rest wait in the menu, in order.
  expect([...menu.querySelectorAll('[data-focus-key]')]).toEqual(controls.slice(3));
  more.click(); controls.at(-1)!.click(); expect(calls).toEqual(["action_8"]);
  width(1280);
  expect(new Set(host.querySelectorAll('[data-focus-key]')).size).toBe(8);
});

/// (partial: responsive focus reconciliation and lifetime cleanup)
test("resize returns a moved focused action to its opener and disposed layouts stop reacting", () => {
  width(1280); const {host} = mount("header", 3);
  host.querySelectorAll<HTMLButtonElement>("button")[2]!.focus();
  width(390);
  expect(document.activeElement).toBe(host.querySelector('[aria-haspopup="menu"]'));
  disposals.splice(0).forEach(stop => stop());
  const before = host.innerHTML; width(1280); expect(host.innerHTML).toBe(before);
});

/// (partial: controls moved out of an open menu regain their original semantics)
test("desktop resize removes menuitem semantics from actions that return to the toolbar", () => {
  width(390); const {host} = mount("header", 3);
  host.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!.click();
  expect(host.querySelectorAll('[role="menuitem"]')).toHaveLength(2);
  width(1280);
  expect(host.querySelector('[role="menuitem"]')).toBeNull();
  expect(host.querySelectorAll(".form-header-actions > button")).toHaveLength(3);
});

test.each([
  ["header", 390, 1280, 2],
  ["header", 1280, 390, 0],
  ["header", 390, 1280, 0],
  ["button_box", 390, 1280, 2],
  ["button_box", 1280, 900, 0]
] as const)("%s resize %i -> %i retains original action %i when it becomes or remains direct", (kind, before, after, index) => {
  width(before); const { host } = mount(kind, kind === "header" ? 3 : 8);
  const focused = host.querySelectorAll<HTMLButtonElement>("[data-focus-key]")[index]!;
  if (focused.closest('[role="menu"]')) host.querySelector<HTMLButtonElement>('[aria-haspopup="menu"]')!.click();
  focused.focus(); expect(document.activeElement).toBe(focused);
  width(after);
  expect(focused.isConnected).toBe(true); expect(focused.disabled).toBe(false);
  expect(focused.closest('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(focused);
});

test("a statistic action moving into overflow still returns focus to the More opener", () => {
  width(1280); const { host } = mount("button_box", 8);
  const focused = host.querySelectorAll<HTMLButtonElement>("[data-focus-key]")[4]!; focused.focus();
  width(900);
  expect(focused.closest('[role="menu"]')).not.toBeNull();
  expect(document.activeElement).toBe(host.querySelector('[aria-haspopup="menu"]'));
});

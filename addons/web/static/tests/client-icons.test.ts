/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { DEFAULT_MENU_ICON, menuIcon, menuIconChoices } from "../src/client/icons.js";
import { actionButton, button, isActionLabel } from "../src/client/dom.js";
import { fieldWidgets } from "../src/client/registry.js";
import { menuIconPicker } from "../src/client/menu-icon-picker.js";
import type { Field } from "../src/client/metadata.js";

afterEach(() => { vi.restoreAllMocks(); });

test("menu icon names cannot inject markup, attributes or external resources; unknown names show the default icon", () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  for (const name of ["<script>alert(1)</script>", "https://evil.example/icon", "__proto__", "constructor", "no-such-icon"]) {
    const svg = menuIcon(`lucide:${name}`);
    expect(svg.dataset["icon"]).toBe(DEFAULT_MENU_ICON);
    expect(svg.dataset["requestedIcon"]).toBe("unknown");
    expect(svg.outerHTML).not.toContain(name);
    expect(svg.querySelector("image,use,foreignObject,script")).toBeNull();
  }
  expect(warn).toHaveBeenCalledTimes(5);
  menuIcon("lucide:no-such-icon");
  expect(warn).toHaveBeenCalledTimes(5); // reported once per value, not on every render
  expect(menuIcon(null).dataset["icon"]).toBe("layout-grid");
  expect(menuIcon("lucide:settings").dataset["icon"]).toBe("settings");
  expect(menuIcon("lucide:library").dataset["icon"]).toBe("library");
});

const iconField: Field = { name: "web_icon", label: "Web Icon", kind: "char", required: false, readonly: false, selection: null, relation: null, relationField: null };
function picker(value: unknown, field: Field = iconField): { node: HTMLElement; change: ReturnType<typeof vi.fn> } {
  const change = vi.fn();
  const handle = menuIconPicker({ field, value, change, rpc: undefined as never, context: {}, run: job => { void job(); }, report: () => {} });
  document.body.replaceChildren(handle.node);
  return { node: handle.node, change };
}

test("the menu icon picker is a registered char widget that stores lucide:<name> and clears to the default", () => {
  expect(fieldWidgets.get("web.menu_icon").kinds).toEqual(["char"]);
  const { node, change } = picker("lucide:settings");
  expect(node.querySelector(".menu-icon-preview svg")?.getAttribute("data-icon")).toBe("settings");
  const toggle = node.querySelector<HTMLButtonElement>(".menu-icon-toggle")!;
  const panel = node.querySelector<HTMLElement>(".menu-icon-panel")!;
  expect(panel.hidden).toBe(true);
  toggle.click();
  expect(panel.hidden).toBe(false);
  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  const choices = [...node.querySelectorAll<HTMLButtonElement>("[data-icon-choice]")];
  expect(choices.map(choice => choice.dataset["iconChoice"])).toEqual([...menuIconChoices()]);
  expect(node.querySelector('[data-icon-choice="settings"]')?.getAttribute("aria-pressed")).toBe("true");
  node.querySelector<HTMLButtonElement>('[data-icon-choice="library"]')!.click();
  expect(change).toHaveBeenLastCalledWith("lucide:library");
  node.querySelector<HTMLButtonElement>(".menu-icon-reset")!.click();
  expect(change).toHaveBeenLastCalledWith(false);
});

test("the menu icon picker filters by name, reports no match, and shows unknown and empty values honestly", () => {
  vi.spyOn(console, "warn").mockImplementation(() => {});
  const { node } = picker("lucide:not-shipped");
  expect(node.querySelector(".menu-icon-preview svg")?.getAttribute("data-icon")).toBe(DEFAULT_MENU_ICON);
  expect(node.textContent).toContain("not shipped with this version");
  const search = node.querySelector<HTMLInputElement>(".menu-icon-search")!;
  search.value = "book"; search.dispatchEvent(new Event("input"));
  const visible = [...node.querySelectorAll<HTMLButtonElement>("[data-icon-choice]")].filter(choice => !choice.hidden).map(choice => choice.dataset["iconChoice"]);
  expect(visible.length).toBeGreaterThan(0);
  expect(visible.every(name => name!.includes("book"))).toBe(true);
  search.value = "zzz-nothing"; search.dispatchEvent(new Event("input"));
  expect(node.querySelector<HTMLElement>(".menu-icon-panel .menu-icon-note")!.hidden).toBe(false);
  const empty = picker(false);
  expect(empty.node.querySelector(".menu-icon-name")?.textContent).toBe(`Default (${DEFAULT_MENU_ICON})`);
  expect(empty.node.querySelector<HTMLButtonElement>(".menu-icon-reset")!.disabled).toBe(true);
  const readonly = picker("lucide:library", { ...iconField, readonly: true });
  expect(readonly.node.querySelector(".menu-icon-toggle")).toBeNull();
  expect(readonly.node.querySelector(".menu-icon-panel")).toBeNull();
});

test("action icons preserve visible labels, button type and one activation", () => {
  const click = vi.fn();
  const control = actionButton("Save", click, "primary");
  expect(control.textContent).toBe("Save");
  expect(control.type).toBe("button");
  expect(control.querySelector('svg[data-icon="save"]')).not.toBeNull();
  control.click();
  expect(click).toHaveBeenCalledOnce();
  const actions = actionButton("Actions", click, "action-toggle-btn");
  expect(actions.textContent).toBe("Actions");
  expect(actions.querySelectorAll("svg")).toHaveLength(1);
  expect(actions.lastElementChild?.getAttribute("data-icon")).toBe("chevron-down");
});

test("a data label that matches a command name gets no command icon", () => {
  // A menu named "Actions" (Technical > Actions) or an arch button named "Delete" is data, not the built-in command.
  for (const label of ["Actions", "Delete", "New", "Search"]) {
    const item = button(label, () => {}, "dropdown-item");
    expect(item.textContent).toBe(label);
    expect(item.querySelector("svg")).toBeNull();
    expect(item.classList.contains("has-action-icon")).toBe(false);
  }
  expect(isActionLabel("Actions")).toBe(true);
  expect(isActionLabel("Window Actions")).toBe(false);
});

test("unknown icon diagnostics retain a bounded recent window and truncate large values", () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  menuIcon("lucide:bounded-first");
  for (let index = 0; index < 128; index++) menuIcon(`lucide:bounded-${index}`);
  warn.mockClear();
  menuIcon("lucide:bounded-first");
  expect(warn).toHaveBeenCalledTimes(1);
  warn.mockClear();
  menuIcon(`lucide:${"x".repeat(10_000)}`);
  expect(String(warn.mock.calls[0]?.[0]).length).toBeLessThan(500);
});

test("picker groups native controls without listbox semantics or a multi-control label", () => {
  const { node } = picker("lucide:library");
  expect(node.tagName).not.toBe("LABEL");
  const group = node.querySelector('[role="group"]');
  expect(group?.getAttribute("aria-label")).toBe("Web Icon choices");
  expect(node.querySelector('[role="listbox"],[role="option"]')).toBeNull();
  expect(node.querySelector('[data-icon-choice="library"]')?.getAttribute("aria-pressed")).toBe("true");
  const toggle = node.querySelector<HTMLButtonElement>(".menu-icon-toggle")!;
  expect(toggle.getAttribute("aria-controls")).toBe(node.querySelector(".menu-icon-panel")?.id);
  toggle.click();
  expect(document.activeElement).toBe(node.querySelector(".menu-icon-search"));
  node.querySelector<HTMLInputElement>(".menu-icon-search")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(document.activeElement).toBe(toggle);
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
});

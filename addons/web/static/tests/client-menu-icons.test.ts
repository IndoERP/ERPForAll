/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { actionButton, relabelActionButton } from "../src/client/dom.js";
import { ActionMenus } from "../src/client/action-menus.js";
import { renderCogMenu } from "../src/client/cog-menu.js";
import { renderDebugMenu } from "../src/client/debug-menu.js";
import { renderUserMenuItems } from "../src/client/user-menu.js";
import { DEFAULT_MENU_ICON, menuEntryIcons } from "../src/client/icons.js";

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

/** The rule for every menu: all entries carry an icon, or none does (so the label column is one column). */
function expectUniform(entries: readonly Element[], label: string): void {
  expect(entries.length, `${label} has entries`).toBeGreaterThan(1);
  const withIcon = entries.filter(entry => entry.querySelector(":scope > .erp-icon, :scope > * > .erp-icon") !== null);
  expect(withIcon.length === 0 || withIcon.length === entries.length, `${label}: ${withIcon.length} of ${entries.length} entries have an icon (${entries.map(entry => entry.textContent).join(", ")})`).toBe(true);
}
const iconNames = (entries: readonly Element[]) => entries.map(entry => entry.querySelector(".erp-icon")?.getAttribute("data-icon") ?? null);

test("the built-in form Actions entries all carry an icon and Unarchive has its own", () => {
  const entries = (["Reload", "Archive", "Delete"] as const).map(label => actionButton(label, () => {}, "dropdown-item"));
  expectUniform(entries, "form Actions menu");
  expect(new Set(iconNames(entries)).size).toBe(3);
  const archive = entries[1]!;
  expect(iconNames([archive])).toEqual(["archive"]);
  relabelActionButton(archive, "Unarchive");
  expect(archive.textContent).toBe("Unarchive");
  expect(iconNames([archive])).toEqual(["archive-restore"]);
  expect(archive.querySelectorAll(".erp-icon")).toHaveLength(1);
});

test("menuEntryIcons: none when no entry names one, every entry when one does, unknown names fall back loudly", () => {
  expect(menuEntryIcons([undefined, null])).toEqual([null, null]);
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const mixed = menuEntryIcons(["settings", undefined, "no-such-icon"]);
  expect(mixed.map(svg => svg?.getAttribute("data-icon"))).toEqual(["settings", DEFAULT_MENU_ICON, DEFAULT_MENU_ICON]);
  expect(warn).toHaveBeenCalledOnce();
  expect(menuEntryIcons([undefined], true)[0]?.getAttribute("data-icon")).toBe(DEFAULT_MENU_ICON);
});

test("list Actions menu: an entry without an icon gets the default one when a sibling names one, and none when no sibling does", () => {
  const env = { resModel: "res.partner", getActiveIds: () => [1] };
  for (const [items, expected] of [
    [[{ id: 1, label: "Export", icon: "files" }, { id: 2, label: "Archive" }, { id: 3, label: "Delete" }], "all"],
    [[{ id: 1, label: "Export" }, { id: 2, label: "Archive" }], "none"],
  ] as const) {
    const node = new ActionMenus(items, env).render(); document.body.append(node);
    const entries = [...node.querySelectorAll(".action-menu-item")];
    expectUniform(entries, "list Actions menu");
    expect(entries.every(entry => (entry.querySelector(".erp-icon") !== null) === (expected === "all"))).toBe(true);
    node.remove();
  }
});

test("cog menu entries share one icon rule", async () => {
  const handle = renderCogMenu([{ key: "a", label: "Print", icon: "printer" }, { key: "b", label: "Duplicate" }, { key: "c", label: "Export", icon: "files" }], {});
  document.body.append(handle.node);
  await vi.waitFor(() => expect(handle.node.querySelectorAll(".cog-menu-item")).toHaveLength(3));
  const entries = [...handle.node.querySelectorAll(".cog-menu-item")];
  expectUniform(entries, "cog menu");
  expect(iconNames(entries)).toEqual(["printer", DEFAULT_MENU_ICON, "files"]);
  handle.dispose();
  const plain = renderCogMenu([{ key: "a", label: "One" }, { key: "b", label: "Two" }], {});
  document.body.append(plain.node);
  await vi.waitFor(() => expect(plain.node.querySelectorAll(".cog-menu-item")).toHaveLength(2));
  expectUniform([...plain.node.querySelectorAll(".cog-menu-item")], "plain cog menu");
  expect(plain.node.querySelector(".erp-icon")).toBeNull();
  plain.dispose();
});

test("user menu custom entries always carry an icon, like the built-in Preferences and Sign out", () => {
  const container = document.createElement("div"); document.body.append(container);
  const handle = renderUserMenuItems(container, [
    { id: "help", description: "Help", icon: "book" },
    { id: "plain", description: "Documentation" },
    { type: "separator" },
    { id: "dark", type: "switch", description: "Dark mode" },
  ], {});
  const entries = [...container.querySelectorAll('[data-menu="help"], [data-menu="plain"], .user-menu-switch-row')];
  expect(entries).toHaveLength(3);
  expectUniform(entries, "user menu");
  expect(iconNames(entries)).toEqual(["book", DEFAULT_MENU_ICON, DEFAULT_MENU_ICON]);
  handle.dispose();
});

test("debug menu decides once for all its sections", async () => {
  const container = document.createElement("div"); document.body.append(container);
  const sections = { get: () => ({ label: "S", sequence: 1 }), has: () => true };
  const handle = await renderDebugMenu(container, [() => [
    { id: "a", section: "one", description: "Fields", icon: "list" },
    { id: "b", section: "two", description: "Views" },
    { id: "c", section: "two", description: "Rules", icon: "shield" },
  ]], sections, {});
  const entries = [...container.querySelectorAll(".debug-menu-item")];
  expectUniform(entries, "debug menu");
  expect(iconNames(entries)).toEqual(["list", DEFAULT_MENU_ICON, "shield"]);
  handle.dispose();
});

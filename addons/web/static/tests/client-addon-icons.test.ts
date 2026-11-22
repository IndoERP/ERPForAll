/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { addonsView, settingsView } from "../src/client/services.js";
import { RpcClient } from "../src/client/transport.js";
import type { SettingsRootMenu } from "../src/client/settings-items.js";

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });
const item = (app: string) => ({ kind: "info", key: `${app}.info`, app, block: "General", label: "Visible", sequence: 0, value: "", detail: null });
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });

async function settings(roots: readonly SettingsRootMenu[], catalog: () => Response) {
  const calls: string[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)), method = String(path).split("/").at(-1)!;
    calls.push(method);
    if (method === "erp_available_addons") return catalog();
    return json(method === "erp_describe" ? { wire_major: 1, registry_version: "r1", scope: args.scope, declarations: [], items: [item("theme_office_blue"), item("notes")] } : { wire_major: 1, scope: args.scope, revision: "r1", values: [] });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], "r1", undefined, undefined, null, { navigation: { roots, openTarget: async () => {} } });
  return { host, calls };
}
const notesRoot: SettingsRootMenu = { id: 2, module: "notes", label: "Notes", icon: "lucide:notebook-pen" };
const catalog = () => json({ wire_major: 1, registry_version: "r1", addons: [{ name: "theme_office_blue", icon: "lucide:palette" }, { name: "web" }] });

/// (partial: the Settings sidebar takes the addon icon for an app without a root menu)
test("an app without a root menu shows its addon's manifest icon; an app with one keeps its menu icon", async () => {
  const { host, calls } = await settings([notesRoot], catalog);
  const theme = [...host.querySelectorAll(".settings-app-tab")].find(tab => tab.textContent?.includes("Theme Office Blue"))!;
  expect(theme.querySelector('.settings-app-icon [data-icon="palette"]')).not.toBeNull();
  expect(theme.querySelector("[data-letter]")).toBeNull();
  const notes = [...host.querySelectorAll(".settings-app-tab")].find(tab => tab.textContent?.includes("Notes"))!;
  expect(notes.querySelector('[data-icon="notebook-pen"]')).not.toBeNull();
  expect(calls.filter(method => method === "erp_available_addons")).toHaveLength(1);
});

/// (partial: no catalogue read when every app has a root menu; a refused read shows the default icon)
test("the catalogue is read only for an app without a root menu, and a refusal shows the default icon with a warning", async () => {
  const onlyNotes = await settings([notesRoot, { id: 3, module: "theme_office_blue", label: "Theme", icon: "lucide:palette" }], catalog);
  expect(onlyNotes.calls).not.toContain("erp_available_addons");
  document.body.replaceChildren();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const refused = await settings([notesRoot], () => json({ message: "Access refused." }, 403));
  const theme = [...refused.host.querySelectorAll(".settings-app-tab")].find(tab => tab.textContent?.includes("Theme Office Blue"))!;
  expect(theme.querySelector('.settings-app-icon svg[data-icon="layout-grid"]')).not.toBeNull();
  expect(warn).toHaveBeenCalledWith(expect.stringContaining("Could not load Settings app icons"));
});

/// (partial: an Apps tile shows the declared icon, and its letter only when none is declared)
test("an Apps tile shows the declared icon instead of its letter", async () => {
  const row = (fields: Record<string, unknown>) => ({ version: "1", state: "installed", installable: true, summary: "", category: "Productivity", author: "ERPForAll", application: true, website: "", license: "MIT", depends: [], ...fields });
  const rpc = new RpcClient(async () => json({ wire_major: 1, registry_version: "r1", addons: [row({ name: "theme_office_blue", label: "Office Blue Theme", icon: "lucide:palette" }), row({ name: "uom", label: "Units" })] }), () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});
  const tiles = [...host.querySelectorAll(".addon-icon")];
  const theme = tiles.find(tile => tile.querySelector('[data-icon="palette"]'));
  expect(theme?.classList.contains("addon-icon-glyph")).toBe(true);
  expect(theme?.hasAttribute("data-letter")).toBe(false);
  expect(tiles.some(tile => tile.getAttribute("data-letter") === "U")).toBe(true);
});

test("a Settings app whose root menu names an addon SVG shows that image in the sidebar", async () => {
  const { host } = await settings([{ ...notesRoot, icon: "notes,static/description/icon.svg" }], catalog);
  const notes = [...host.querySelectorAll(".settings-app-tab")].find(tab => tab.textContent?.includes("Notes"))!;
  expect(notes.querySelector(".settings-app-icon img")?.getAttribute("src")).toBe("/notes/static/description/icon.svg");
});

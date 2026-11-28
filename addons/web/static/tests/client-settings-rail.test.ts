/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { readSettingsItems, readSettingsTarget, readMenuModule } from "../src/client/settings-items.js";
import { settingsView } from "../src/client/services.js";
import { RpcClient } from "../src/client/transport.js";
import type { SettingsRootMenu } from "../src/client/settings-items.js";
const item = { kind: "info", key: "notes.info", app: "notes", block: "General", label: "Visible", sequence: 0, value: "", detail: null };
afterEach(() => document.body.replaceChildren());
test.each([
  [{ ...item, value: null }, "Invalid setting item value."],
  [{ ...item, value: Infinity }, "Invalid setting item value."],
  [{ ...item, value: Number.MAX_SAFE_INTEGER + 1 }, "Invalid setting item value."],
  [{ ...item, sequence: -1 }, "Invalid setting item sequence."],
  [{ ...item, sequence: 0x1_0000_0000 }, "Invalid setting item sequence."],
  [{ ...item, app: "" }, "Invalid setting item app."],
  [{ ...item, label: "é".repeat(129) }, "Invalid setting item label."],
  [{ ...item, label: "Line\nBreak" }, "Invalid setting item label."],
  [{ ...item, detail: "é".repeat(513) }, "Invalid setting item detail."],
  [{ ...item, detail: "tab\t" }, "Invalid setting item detail."],
  [{ ...item, key: "wrong-key" }, "Invalid setting item key or app."],
])("invalid scalar or placement is rejected (%j)", (row, message) => {
  expect(() => readSettingsItems({ items: [row] }, [])).toThrowError(expect.objectContaining({ kind: "server", message }));
});
test("decoder accepts scalar boundaries and checks collisions against editable declarations", () => {
  for (const value of [false, 0, -1.5, Number.MAX_SAFE_INTEGER, "", "東京"]) expect(readSettingsItems({ items: [{ ...item, value, label: "é".repeat(128), detail: "é".repeat(512) }] }, [])[0]).toMatchObject({ value });
  expect(() => readSettingsItems({ items: [item] }, ["notes.info"])).toThrowError(expect.objectContaining({ kind: "server", message: "Duplicate settings item or declaration key: notes.info." }));
});
test.each([0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1, "1"])("unsafe record target is refused (%j)", id => {
  expect(() => readSettingsTarget({ kind: "action", id: 7, res_id: id })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid setting target id." }));
});
test("menu module is required nullable provenance and cannot silently default", () => {
  expect(readMenuModule(null)).toBeNull(); expect(readMenuModule("notes")).toBe("notes");
  for (const raw of [undefined, 1, ""]) expect(() => readMenuModule(raw)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid menu module." }));
});
async function rail(roots: readonly SettingsRootMenu[]) {
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body));
    return new Response(JSON.stringify(String(path).endsWith("erp_describe") ? { wire_major: 1, registry_version: "r1", scope: args.scope, declarations: [], items: [item] } : { wire_major: 1, scope: args.scope, revision: "r1", values: [] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], "r1", undefined, undefined, null, { navigation: { roots, openTarget: async () => {} } });
  return host;
}
/// (partial: rail root identity and explicit fallback)
test("rail uses first matching root and preserves Unicode labels with the shell icon", async () => {
  const host = await rail([{ id: 1, module: null, label: "Notes", icon: null }, { id: 2, module: "notes", label: "東京 Notes", icon: "lucide:notebook-pen" }, { id: 3, module: "notes", label: "Later", icon: null }]);
  expect(host.querySelector(".settings-app-name")?.textContent).toBe("東京 Notes");
  expect(host.querySelector('.settings-app-icon [data-icon="notebook-pen"]')).not.toBeNull();
});
test("null ownership and label similarity do not replace the settings-only fallback", async () => {
  const host = await rail([{ id: 1, module: null, label: "Notes", icon: "lucide:settings" }, { id: 2, module: "other", label: "Notes", icon: null }]);
  expect(host.querySelector(".settings-app-name")?.textContent).toBe("Notes");
  expect(host.querySelector('.settings-app-icon [data-icon="layout-grid"]')).not.toBeNull();
  expect(host.querySelector(".settings-app-icon[data-letter]")).toBeNull();
});

test("Inventory and general entry share all authorized app tabs across scopes", async () => {
  const readScopes: string[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const { scope } = JSON.parse(String(init?.body));
    const app = scope.kind === "global" ? "base" : "stock";
    let value;
    if (String(path).endsWith("erp_describe")) {
      value = { wire_major: 1, registry_version: "r1", scope, declarations: [], items: [{ ...item, key: app + ".info", app }] };
    } else {
      readScopes.push(scope.kind);
      value = { wire_major: 1, scope, revision: "1", values: [] };
    }
    return new Response(JSON.stringify(value), {headers: {"Content-Type":"application/json"}});
  }, () => {});
  for (const entry of ["stock", null]) {
    const host = document.createElement("div");
    let pending = Promise.resolve();
    const mounted = await settingsView(host, rpc, [{ id: 1, name: "Store" }], job => { pending = job(); }, () => {},
      {}, ["global", "company"], "r1", undefined, undefined, entry,
      { navigation: { roots: ["base", "stock"].map((module,i)=>({id:i+1,module,label:i ? "Inventory" : "Settings", icon:null})), openTarget: async () => {} } });
    expect([...host.querySelectorAll("[data-app]")].map(tab => tab.getAttribute("data-app"))).toEqual(["base","stock"]);
    expect(host.querySelector('[aria-selected="true"]')?.getAttribute("data-app")).toBe(entry ?? "base");
    const other = entry ? "base" : "stock";
    const picker = host.querySelector<HTMLSelectElement>(".settings-category-picker select")!;
    if (entry) host.querySelector<HTMLButtonElement>(`[data-app="${other}"]`)!.click();
    else {
      picker.value = other;
      picker.dispatchEvent(new Event("change"));
      expect(picker.value).toBe("base");
    }
    await pending;
    expect(host.querySelector('[aria-selected="true"]')?.getAttribute("data-app")).toBe(other);
    expect(picker.value).toBe(other);
    expect(readScopes.at(-1)).toBe(entry ? "global" : "company");
    mounted.dispose?.();
  }
});

test("module toggle items decode all three states and reject malformed payloads", () => {
  const base = { kind: "module", key: "stock.module_delivery", app: "stock", block: "Shipping", label: "Carriers", sequence: 200, addon: "delivery" };
  for (const state of ["installed", "not_installed", "not_available"] as const) {
    expect(readSettingsItems({ items: [{ ...base, state }] }, [])).toEqual([{ ...base, state }]);
  }
  expect(() => readSettingsItems({ items: [{ ...base, state: "installed" }] }, ["stock.module_delivery"])).toThrowError(
    expect.objectContaining({ kind: "server", message: "Duplicate settings item or declaration key: stock.module_delivery." })
  );
  for (const [bad, kind, message] of [
    [{ ...base, state: "installed", extra: 1 }, "unsupported", "Unknown setting item member: extra."],
    [{ kind: "module", key: "stock.module_delivery", app: "stock", block: "Shipping", label: "Carriers", sequence: 200, state: "installed" }, "server", "Missing setting item member: addon."],
    [{ ...base }, "server", "Missing setting item member: state."],
    [{ ...base, addon: "Bad-Addon", state: "installed" }, "server", "Invalid setting item addon."],
    [{ ...base, addon: "", state: "installed" }, "server", "Invalid setting item addon."],
    [{ ...base, state: "pending" }, "server", "Invalid setting item module state."],
  ] as const) {
    expect(() => readSettingsItems({ items: [bad] }, [])).toThrowError(expect.objectContaining({ kind, message }));
  }
});

test("cancelling a category scope switch keeps the current app, edit and picker focus", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  const rpc = new RpcClient(async (path, init) => {
    if (String(path).endsWith("erp_available_addons")) return new Response(JSON.stringify({addons: []}), { headers: {"Content-Type":"application/json"} });
    const { scope } = JSON.parse(String(init?.body));
    const app = scope.kind === "global" ? "base" : "stock";
    const key = app + ".enabled";
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope, declarations: [{ key, app, label: "Enabled", block: "General",
        kind: "boolean", default: false, choices: null, writable: true, sequence: 1 }] }
      : { wire_major: 1, scope, revision: "1", values: [{ key, value: false, source: "default" }] };
    return new Response(JSON.stringify(response), { headers: {"Content-Type":"application/json"} });
  }, () => {});
  const host = document.body.appendChild(document.createElement("div"));
  let pending = Promise.resolve();
  const mounted = await settingsView(host, rpc, [{id: 1, name: "Demo"}], job => { pending = job(); }, () => {}, {}, ["global","company"], "r1");
  const field = host.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  field.click();
  const picker = host.querySelector<HTMLSelectElement>(".settings-category-picker select")!;
  picker.focus(); picker.value = "stock"; picker.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")).not.toBeNull());
  const cancel = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(button => button.textContent === "Cancel")!;
  cancel.focus(); cancel.click(); await pending;
  expect(picker.value).toBe("base");
  expect(mounted.dirty()).toBe(true);
  expect(field.checked).toBe(true);
  expect(document.activeElement).toBe(picker);
  mounted.dispose?.();
});

test("group toggle items decode every state, a reason only when unavailable, and refuse unknown members, states and groups", () => {
  const base = { kind: "group", key: "stock.group_stock_tracking_lot", app: "stock", block: "Operations", label: "Packages", sequence: 100, group: "stock.group_tracking_lot", on_for_all: false };
  const reason = "Setting item stock.group_stock_tracking_lot cannot toggle the group stock.group_tracking_lot: it does not exist.";
  for (const row of [{ ...base, state: "enabled", reason: null }, { ...base, state: "disabled", reason: null }, { ...base, state: "not_available", reason }, { ...base, state: "not_available", reason, on_for_all: true }]) {
    expect(readSettingsItems({ items: [row] }, [])).toEqual([row]);
  }
  for (const [bad, kind, message] of [
    [{ ...base, state: "enabled", reason: null, group_id: 41 }, "unsupported", "Unknown setting item member: group_id."],
    [{ ...base, reason: null }, "server", "Missing setting item member: state."],
    [{ ...base, state: "enabled" }, "server", "Missing setting item member: reason."],
    [(({ on_for_all: _, ...rest }) => ({ ...rest, state: "enabled", reason: null }))(base), "server", "Missing setting item member: on_for_all."],
    [{ ...base, state: "on", reason: null }, "server", "Invalid setting item group state."],
    [{ ...base, state: "not_available", reason: null }, "server", "Invalid setting item group reason."],
    [{ ...base, state: "enabled", reason }, "server", "Invalid setting item group reason."],
    [{ ...base, state: "enabled", reason: null, on_for_all: true }, "server", "Invalid setting item group on_for_all."],
    [{ ...base, state: "not_available", reason, on_for_all: "yes" }, "server", "Invalid setting item group on_for_all."],
    [{ ...base, state: "not_available", reason: "line\u0007" }, "server", "Invalid setting item group reason."],
    [{ ...base, group: "tracking_lot", state: "enabled", reason: null }, "server", "Invalid setting item group."],
  ] as const) {
    expect(() => readSettingsItems({ items: [bad] }, [])).toThrowError(expect.objectContaining({ kind, message }));
  }
});

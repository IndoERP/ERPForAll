/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { configureClientTemplates } from "../src/client/registry.js";
const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true }, companies: [{ id: 1, name: "One" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "fixture" } };
const form = { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "web.settings", options: { scope_kinds: ["global"], app: null }, modifiers: { readonly: null, required: null, invisible: null } }] };
const settingsViews = { wire_major: 1, registry_version: "r1", description_version: "d1", model: "ir.config.setting", fields: { "ir.config.setting": {} }, notices: [], views: [form] };
const recordViews = { ...settingsViews, model: "res.company", fields: { "res.company": {} }, views: [{ ...form, nodes: [{ kind: "form", record_mode: "record", children: [] }] }] };
let app: HumanApp | undefined;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  configureClientTemplates([{ name: "web.HumanControlPanel", render: () => { const root = document.createElement("section"); root.setAttribute("data-control-panel", ""); for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const child = document.createElement("div"); child.setAttribute(`data-control-${key}`, ""); root.append(child); } return root; } }]);
});
afterEach(() => { app?.dispose(); app = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); history.replaceState(null, "", "/"); });
async function start(target: unknown, options: { deny?: boolean; dirty?: boolean; writeGate?: Promise<void>; ambiguous?: boolean; duplicateRoots?: boolean; childOnly?: boolean } = {}) {
  history.replaceState(null, "", "/erp/action-21?view_type=form");
  const log: { method: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body)) as Record<string, unknown>, method = path.split("/").at(-1)!;
    log.push({ method, body });
    if (options.deny && method === "erp_load_web_action_views" && (body["ids"] as number[] | undefined)?.[0] === 7) return new Response(JSON.stringify({ message: "Company access refused." }), { status: 403, headers: { "Content-Type": "application/json" } });
    if (method === "erp_write_values" && options.writeGate) await options.writeGate;
    if (method === "erp_write_values" && options.ambiguous) return new Response("null", { headers: { "Content-Type": "application/json" } });
    const actionId = Number((body["ids"] as number[] | undefined)?.[0]), action = { wire_major: 1, id: actionId, name: actionId === 21 ? "Settings" : "Company", model: actionId === 21 ? "ir.config.setting" : "res.company", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null };
    const value = method === "erp_load_web_menus" ? { wire_major: 1, registry_version: "r1", roots: [1, 2], nodes: [{ id: 1, parent_id: null, children: [], action_id: 21, label: "Administration", icon: "lucide:settings", module: options.childOnly ? null : "base" }, { id: 2, parent_id: null, children: [3], action_id: null, label: "Company tools", icon: null, module: options.duplicateRoots ? "base" : null }, { id: 3, parent_id: 2, children: [], action_id: 7, label: "Company", icon: null, module: "base" }] }
      : method === "get_res_users_settings" ? { homemenu_config: options.duplicateRoots ? "[2,1]" : null }
      : method === "erp_load_web_action_views" ? { action, views: actionId === 21 ? settingsViews : recordViews }
      : method === "erp_load_web_action" ? action
      : method === "get_views" ? recordViews
      : method === "erp_describe" ? { wire_major: 1, registry_version: "r1", scope: body["scope"], declarations: options.dirty ? [{ key: "base.flag", label: "Enabled", app: "base", block: "Users", sequence: 1, kind: "boolean", choices: null, writable: true }] : [], items: [{ kind: "link", key: "base.go", label: "Update Info", app: "base", block: "Companies", sequence: 20, target }] }
      : method === "erp_read_values" || method === "erp_write_values" ? { wire_major: 1, scope: body["scope"], revision: "r1", values: options.dirty ? [{ key: "base.flag", value: false, source: "stored" }] : [] }
      : method === "web_read" ? [{ id: 9, display_name: "One" }]
      : method === "default_get" ? {}
      : method === "onchange" ? { value: {} } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = document.createElement("div"); for (const name of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${name}`, ""); root.append(node); } document.body.append(root);
  app = new HumanApp(root, bootstrap); await app.start();
  return { root, log, link: () => root.querySelector<HTMLButtonElement>('[data-setting-key="base.go"] button')! };
}

/// (partial: mounted shell record and menu routing)
test("Settings record links use the ordinary authorized action loader and record route", async () => {
  const { link, log } = await start({ kind: "action", id: 7, res_id: 9 }); link().click();
  await vi.waitFor(() => expect(location.pathname).toContain("action-7"));
  expect(location.search).toContain("id=9");
  expect(log.some(call => call.method === "erp_load_web_action_views" && (call.body["ids"] as number[] | undefined)?.[0] === 7)).toBe(true);
  expect(log.some(call => call.method === "erp_write_values")).toBe(false);
});

test("Settings menu links use the authorized first actionable descendant", async () => {
  const { link } = await start({ kind: "menu", id: 2 }); link().click();
  await vi.waitFor(() => expect(location.pathname).toContain("action-7"));
  expect(location.search).toContain("menu=3");
});

test("dirty cancellation preserves Settings and edits before any target action load", async () => {
  const { root, link, log } = await start({ kind: "action", id: 7, res_id: 9 }, { dirty: true });
  const field = root.querySelector<HTMLInputElement>('input[type="checkbox"]')!; field.checked = true; field.dispatchEvent(new Event("change")); link().click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")).not.toBeNull());
  [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(button => button.textContent === "Cancel")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")).toBeNull());
  expect(location.pathname).toContain("action-21"); expect(field.checked).toBe(true);
  expect(log.some(call => (call.body["ids"] as number[] | undefined)?.[0] === 7)).toBe(false);
});

test("denied link targets preserve the mounted Settings view", async () => {
  const { root, link } = await start({ kind: "action", id: 7, res_id: 9 }, { deny: true }); link().click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("Company access refused."));
  expect(root.querySelector(".settings-form")).not.toBeNull(); expect(location.pathname).toContain("action-21");
});


test.each(["Save", "Discard"])("dirty %s completes through the shell before leaving Settings", async choice => {
  const { root, link, log } = await start({ kind: "action", id: 7, res_id: 9 }, { dirty: true });
  const field = root.querySelector<HTMLInputElement>('input[type="checkbox"]')!; field.checked = true; field.dispatchEvent(new Event("change")); link().click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")).not.toBeNull());
  [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(button => button.textContent === choice)!.click();
  await vi.waitFor(() => expect(location.pathname).toContain("action-7"));
  const writes = log.filter(call => call.method === "erp_write_values");
  expect(writes).toHaveLength(choice === "Save" ? 1 : 0);
  if (choice === "Save") {
    expect(writes[0]!.body["changes"]).toEqual([{ key: "base.flag", value: true }]);
    expect(log.indexOf(writes[0]!)).toBeLessThan(log.findIndex(call => call.method === "erp_load_web_action_views" && (call.body["ids"] as number[] | undefined)?.[0] === 7));
  }
});

test("pending writes disable links and repeated activation cannot dispatch before the save settles", async () => {
  let release!: () => void; const writeGate = new Promise<void>(resolve => { release = resolve; });
  const { root, link, log } = await start({ kind: "action", id: 7, res_id: 9 }, { dirty: true, writeGate });
  const field = root.querySelector<HTMLInputElement>('input[type="checkbox"]')!; field.checked = true; field.dispatchEvent(new Event("change"));
  [...root.querySelectorAll<HTMLButtonElement>(".settings-toolbar button")].find(button => button.textContent === "Save")!.click();
  try {
    await vi.waitFor(() => expect(log.some(call => call.method === "erp_write_values")).toBe(true));
    expect(link().disabled).toBe(true); link().click(); link().click();
    expect(log.some(call => (call.body["ids"] as number[] | undefined)?.[0] === 7)).toBe(false);
  } finally { release(); }
  await vi.waitFor(() => expect(link().disabled).toBe(false));
  link().click(); link().click();
  await vi.waitFor(() => expect(location.pathname).toContain("action-7"));
  expect(log.filter(call => call.method === "erp_load_web_action_views" && (call.body["ids"] as number[] | undefined)?.[0] === 7)).toHaveLength(1);
});

test("an ambiguous write cannot be replayed by saving through a Settings link", async () => {
  const { root, link, log } = await start({ kind: "action", id: 7, res_id: 9 }, { dirty: true, ambiguous: true });
  const field = root.querySelector<HTMLInputElement>('input[type="checkbox"]')!; field.checked = true; field.dispatchEvent(new Event("change"));
  [...root.querySelectorAll<HTMLButtonElement>(".settings-toolbar button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("could not be confirmed"));
  const close = document.querySelector<HTMLButtonElement>('dialog[open] [aria-label="Close"]');
  if (close) close.click(); else [...document.querySelectorAll<HTMLButtonElement>("dialog[open] button")].find(button => button.textContent === "Close")!.click();
  link().click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("Save your changes before leaving"));
  [...document.querySelectorAll<HTMLButtonElement>("dialog[open] button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("check the previous save"));
  expect(log.filter(call => call.method === "erp_write_values")).toHaveLength(1);
  expect(location.pathname).toContain("action-21");
});

test("an unavailable menu fails clearly without replacing Settings", async () => {
  const { root, link } = await start({ kind: "menu", id: 999 }); link().click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("This Settings menu is unavailable or has no action."));
  expect(root.querySelector(".settings-form")).not.toBeNull(); expect(location.pathname).toContain("action-21");
});

test("personal home-menu order does not change the server-order identity tie", async () => {
  const { root } = await start({ kind: "action", id: 7, res_id: 9 }, { duplicateRoots: true });
  expect(root.querySelector(".settings-app-name")?.textContent).toBe("Administration");
});

test("a child menu's ownership never supplies a root identity", async () => {
  const { root } = await start({ kind: "action", id: 7, res_id: 9 }, { childOnly: true });
  expect(root.querySelector(".settings-app-name")?.textContent).toBe("General Settings");
});

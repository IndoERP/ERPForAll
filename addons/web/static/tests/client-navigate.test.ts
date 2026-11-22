/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shell navigation for addon code (`navigate`, `openAction` on systray, service, client action and field widget
// environments) and the page's companies kept across navigation.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import {
  clientActions, configureClientTemplates, fieldWidgets, registerClientAction, registerFieldWidget, registerServiceController,
  registerSystrayItem, serviceControllers, systrayItems, type ClientActionEnvironment, type ServiceEnvironment, type WidgetEnvironment
} from "../src/client/registry.js";
import type { SystrayEnvironment } from "../src/client/systray.js";
import { HumanApp } from "../src/client/app.js";

const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company One" }, { id: 3, name: "Company Three" }],
  current_company_id: 1, allowed_company_ids: [1, 3], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};
const field = (widget = "char") => ({ kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget, options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" });
const fields = { "test.note": { name: { type: "char", label: "Name", required: false, readonly: false, relation: null, relation_field: null, selection: null } } };
const notes = (formWidget = "char") => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields, notices: [],
  views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field(formWidget)] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field()] }
  ]
});
const service = (widget: string) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields, notices: [],
  views: [{ identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
    nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget, options: {}, modifiers: { readonly: null, required: null, invisible: null } }] }]
});
const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });

type Call = { model: string; method: string; body: Record<string, unknown> };
interface Options {
  /** The user's stored home action (`false`: none). */
  home?: number | false | "fail";
  /** Action ids whose metadata is refused with 403. */
  refused?: number[];
  formWidget?: string;
  /** Action 40 is a service form with this widget. */
  serviceWidget?: string;
  /** Action 50 is a client action with this tag. */
  clientTag?: string;
}
function server(options: Options = {}) {
  const calls: Call[] = [];
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    const parts = url.split("/"), method = parts.at(-1)!, model = parts.at(-2)!;
    const body = init?.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ model, method, body });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10, 11, 12], nodes: [
      { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
      { module: null, id: 11, parent_id: null, label: "Archive", action_id: 21, children: [], icon: null },
      { module: null, id: 12, parent_id: null, label: "Board", action_id: 50, action_kind: "client", children: [], icon: null }] });
    if (model === "res.users" && method === "web_read") {
      if (options.home === "fail") return reply({ name: "AccessError", message: "No access to users" }, 403);
      return reply([{ id: 2, action_id: options.home ?? false }]);
    }
    if (method === "erp_load_web_action_views" || method === "erp_load_web_action") {
      const id = (body["ids"] as number[])[0]!;
      if (options.refused?.includes(id)) return reply({ name: "AccessError", message: "You may not open this action." }, 403);
      if (id === 40) {
        const action = { wire_major: 1, id, name: "Service Page", model: "test.note", target: "current", views: [{ id: 3, type: "form" }], domain: null, context: null };
        return reply(method === "erp_load_web_action_views" ? { action, views: service(options.serviceWidget ?? "missing") } : action);
      }
      const action = { wire_major: 1, id, name: id === 21 ? "Archive" : "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: notes(options.formWidget) } : action);
    }
    if (model === "ir.actions.client" && method === "erp_load_web_client_action") {
      return reply({ wire_major: 1, result: { kind: "client", id: 50, name: "Board", tag: options.clientTag ?? "missing.tag", params: {} } });
    }
    if (method === "erp_load_web_action_ref") {
      return reply({ wire_major: 1, id: 21, name: "Archive", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null });
    }
    if (method === "get_views") return reply(notes(options.formWidget));
    if (method === "web_search_read") return reply({ length: 1, records: [{ id: 5, name: "Agenda" }] });
    if (method === "web_read") return reply([{ id: 5, name: "Agenda" }]);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true } });
    return reply([]);
  });
  vi.stubGlobal("fetch", fetch);
  return { calls, fetch };
}

let app: HumanApp | undefined;
const added: { registry: { remove(key: string): boolean }; key: string }[] = [];
beforeEach(() => configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
  const root = document.createElement("section");
  root.setAttribute("data-control-panel", "");
  for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const node = document.createElement("div"); node.setAttribute(`data-control-${key}`, ""); root.append(node); }
  return root;
} }]));
afterEach(() => {
  app?.dispose(); app = undefined;
  for (const { registry, key } of added.splice(0)) registry.remove(key);
  vi.restoreAllMocks(); vi.unstubAllGlobals();
  document.body.replaceChildren();
  history.replaceState(null, "", "/");
});

function host(): HTMLElement {
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  return root;
}
async function boot(url: string): Promise<HTMLElement> {
  history.replaceState(null, "", url);
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  return root;
}
const address = (): URL => new URL(window.location.href);
function probeSystray(): () => SystrayEnvironment | undefined {
  let captured: SystrayEnvironment | undefined;
  registerSystrayItem("test.navigate_probe", { render(env) { captured = env; return { node: document.createElement("span"), dispose() {} }; } });
  added.push({ registry: systrayItems, key: "test.navigate_probe" });
  return () => captured;
}

test("a menu click keeps the page's companies in the address; a route that names its own keeps those", async () => {
  server();
  await boot("/erp/action-20?cids=1");
  await app!["navigate"]({ action: "21", menu: "11" });
  expect(address().pathname).toBe("/erp/action-21");
  expect(address().searchParams.get("cids")).toBe("1");
  await app!["navigate"]({});
  expect(address().pathname).toBe("/erp");
  expect(address().searchParams.get("cids")).toBe("1");
});

test("an address without companies stays without them", async () => {
  server();
  await boot("/erp/action-20");
  await app!["navigate"]({ action: "21", menu: "11" });
  expect(address().searchParams.has("cids")).toBe(false);
});

test("a systray item navigates through the shell and keeps the companies", async () => {
  const { calls } = server();
  const env = probeSystray();
  await boot("/erp?cids=1");
  await vi.waitFor(() => expect(env()?.navigate).toBeTypeOf("function"));
  await env()!.navigate!({ action: "21" });
  expect(address().pathname).toBe("/erp/action-21");
  expect(address().searchParams.get("cids")).toBe("1");
  expect(calls.some(call => call.method === "web_search_read")).toBe(true);
});

test("addon navigation refuses a malformed route and a method action identity", async () => {
  server();
  const env = probeSystray();
  await boot("/erp");
  await vi.waitFor(() => expect(env()?.navigate).toBeTypeOf("function"));
  await expect(env()!.navigate!({ action: "not-a-number" })).rejects.toMatchObject({ kind: "validation" });
  await expect(env()!.navigate!({ action: "20", method_action: "00000000-0000-4000-8000-000000000000:1" })).rejects.toMatchObject({ kind: "validation" });
  await expect(env()!.navigate!({ action: "20", unknown: "1" } as never)).rejects.toMatchObject({ kind: "validation" });
  expect(address().pathname).toBe("/erp");
});

test("a route that names other companies loads the page again with them, as the company switcher does", async () => {
  server();
  const env = probeSystray();
  await boot("/erp/action-20?cids=1");
  await vi.waitFor(() => expect(env()?.navigate).toBeTypeOf("function"));
  const load = vi.spyOn(HumanApp.prototype as unknown as { navigateLocation(url: string): void }, "navigateLocation").mockImplementation(() => {});
  await env()!.navigate!({ action: "21", cids: ["3", "1"] });
  expect(load).toHaveBeenCalledWith("/erp/action-21?cids=3%2C1");
  // the same companies in another order are the page's own: no reload
  load.mockClear();
  await env()!.navigate!({ action: "21", cids: ["1"] });
  expect(load).not.toHaveBeenCalled();
  expect(address().pathname).toBe("/erp/action-21");
});

test("openAction from the app grid opens the action's list, anchored on the action itself", async () => {
  const { calls } = server();
  const env = probeSystray();
  const root = await boot("/erp?cids=1");
  await vi.waitFor(() => expect(env()?.openAction).toBeTypeOf("function"));
  await env()!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [["name", "=", "Agenda"]], context: { default_name: "New" } });
  expect(calls.find(call => call.method === "erp_load_web_action_ref")?.body).toMatchObject({ xml_id: "test.action_archive" });
  expect(address().pathname).toBe("/erp/action-21");
  expect(address().searchParams.get("view_type")).toBe("list");
  expect(address().searchParams.get("cids")).toBe("1");
  const read = calls.filter(call => call.method === "web_search_read").at(-1)!.body;
  expect(read["domain"]).toEqual([["name", "=", "Agenda"]]);
  expect(read["context"]).toMatchObject({ default_name: "New", allowed_company_ids: [1] });
  expect(root.querySelector("[data-control-breadcrumbs]")?.textContent).toContain("Archive");
});

test("openAction refuses a context key other than default_<field>, before any read", async () => {
  const { calls } = server();
  const env = probeSystray();
  await boot("/erp/action-20");
  await vi.waitFor(() => expect(env()?.openAction).toBeTypeOf("function"));
  await expect(env()!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [], context: { search_default_mine: 1 } })).rejects.toMatchObject({ kind: "validation" });
  await expect(env()!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [], context: { group_by: "name" } })).rejects.toMatchObject({ kind: "validation" });
  expect(address().pathname).toBe("/erp/action-20");
  await expect(env()!.openAction!({ xmlId: "not an id", viewMode: "list", domain: [], context: {} })).rejects.toMatchObject({ kind: "validation" });
  expect(calls.some(call => call.method === "erp_load_web_action_ref")).toBe(false);
});

test("a service controller gets navigate and openAction, which do nothing once its page was left", async () => {
  let env: ServiceEnvironment | undefined;
  registerServiceController("test.navigate_service", { model: "test.note", validateOptions: () => {}, async create(environment) {
    env = environment;
    return { busy: () => false, dirty: () => false, save: async () => {}, discard: () => {}, dispose: () => {} };
  } });
  added.push({ registry: serviceControllers, key: "test.navigate_service" });
  const { fetch } = server({ serviceWidget: "test.navigate_service" });
  await boot("/erp/action-40?cids=1");
  expect(env?.navigate).toBeTypeOf("function");
  await env!.navigate!({ action: "21" });
  expect(address().pathname).toBe("/erp/action-21");
  expect(address().searchParams.get("cids")).toBe("1");
  const before = fetch.mock.calls.length;
  await env!.navigate!({ action: "20" });
  await env!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [], context: {} });
  expect(address().pathname).toBe("/erp/action-21");
  expect(fetch.mock.calls.length).toBe(before);
});

test("a client action gets navigate and openAction with the way back to its screen", async () => {
  let env: ClientActionEnvironment | undefined;
  registerClientAction("test_nav.board", { async create(environment) { env = environment; return { dispose() {} }; } });
  added.push({ registry: clientActions, key: "test_nav.board" });
  server({ clientTag: "test_nav.board" });
  const root = await boot("/erp/action-50?menu=12&cids=1");
  expect(env?.navigate).toBeTypeOf("function");
  await env!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [], context: {} });
  expect(address().pathname).toBe("/erp/action-50");
  expect(address().searchParams.get("method_action")).not.toBeNull();
  expect(address().searchParams.get("cids")).toBe("1");
  expect(root.querySelector("[data-control-breadcrumbs]")?.textContent).toContain("Board");
  await env!.navigate!({ action: "20" });
  // the screen was left when the list opened: its navigation no longer moves the page
  expect(address().pathname).toBe("/erp/action-50");
});

test("form field widgets get navigate and openAction; the way back is the record", async () => {
  let env: WidgetEnvironment | undefined;
  registerFieldWidget("test.navigate_field", { kinds: ["char"], render(environment) { env = environment; return { node: document.createElement("span"), dispose() {} }; } });
  added.push({ registry: fieldWidgets, key: "test.navigate_field" });
  const { calls } = server({ formWidget: "test.navigate_field" });
  const root = await boot("/erp/action-20?id=5&view_type=form&cids=1");
  expect(env?.navigate).toBeTypeOf("function");
  expect(env?.openAction).toBeTypeOf("function");
  await env!.openAction!({ xmlId: "test.action_archive", viewMode: "list", domain: [], context: {} });
  expect(address().pathname).toBe("/erp/action-20");
  expect(address().searchParams.get("method_action")).not.toBeNull();
  expect(address().searchParams.get("cids")).toBe("1");
  expect(calls.some(call => call.method === "web_search_read")).toBe(true);
  expect(root.querySelector("[data-control-breadcrumbs]")?.textContent).toContain("Agenda");
});

test("list cells get navigate and openAction", async () => {
  let env: WidgetEnvironment | undefined;
  registerFieldWidget("list.char", { kinds: ["char"], render(environment) { env = environment; return { node: document.createElement("span"), dispose() {} }; } });
  added.push({ registry: fieldWidgets, key: "list.char" });
  server();
  await boot("/erp/action-20");
  await vi.waitFor(() => expect(env?.navigate).toBeTypeOf("function"));
  expect(env?.openAction).toBeTypeOf("function");
  await env!.navigate!({ action: "21" });
  expect(address().pathname).toBe("/erp/action-21");
});

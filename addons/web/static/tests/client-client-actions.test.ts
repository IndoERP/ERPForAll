/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates, registerClientAction } from "../src/client/registry.js";
import type { ClientActionEnvironment } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import "../../../approvals/static/src/requests.js";

const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Arkana" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};
const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const menus = { wire_major: 1, registry_version: "r1", roots: [10, 11], nodes: [
  { module: null, id: 10, parent_id: null, label: "Board", action_id: 30, action_kind: "client", children: [], icon: null },
  { module: null, id: 11, parent_id: null, label: "Ghost", action_id: 31, action_kind: "client", children: [], icon: null }
] };
const clientAction = (id: number, tag: string) => ({ wire_major: 1, result: { kind: "client", id, name: `Action ${id}`, tag, params: { report: 7 } } });

let app: HumanApp | undefined;
beforeEach(() => configureClientTemplates([{
  name: "web.HumanControlPanel",
  render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
      const node = document.createElement("div");
      node.setAttribute(`data-control-${key}`, "");
      root.append(node);
    }
    return root;
  }
}]));
afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); history.replaceState(null, "", "/"); });
function host(): HTMLElement {
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  return root;
}
function fetchMock(tags: Record<number, string>) {
  return vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_load_web_menus")) return json(menus);
    if (path.endsWith("ir.actions.client/erp_load_web_client_action")) {
      const id = (JSON.parse(String(init?.body)) as { ids: number[] }).ids[0]!;
      return json(clientAction(id, tags[id]!));
    }
    return json([]);
  });
}

/// (partial: registration is unique and validated; the server-side duplicate is a composition error)
test("a client action tag registers once and only in the addon.name form", () => {
  const factory = { create: async () => ({ dispose() {} }) };
  registerClientAction("demo.duplicate", factory);
  expect(() => registerClientAction("demo.duplicate", factory)).toThrowError(expect.objectContaining({ message: "Duplicate client action: demo.duplicate." }));
  for (const bad of ["board", "Demo.board", "demo.Board", "demo.", `demo.${"a".repeat(64)}`]) {
    expect(() => registerClientAction(bad, factory)).toThrowError(expect.objectContaining({ kind: "validation" }));
  }
});

/// (partial: a menu of kind client mounts the registered screen with its params; disposal is once)
test("a client menu mounts its registered screen and disposes it with the page", async () => {
  history.replaceState(null, "", "/erp/action-30?menu=10");
  const seen: ClientActionEnvironment[] = [];
  const dispose = vi.fn();
  registerClientAction("demo.board", { create: async environment => {
    seen.push(environment);
    const node = document.createElement("p"); node.dataset["screen"] = "board"; node.textContent = `report ${String(environment.action.params["report"])}`;
    environment.host.append(node);
    return { dispose, captureState: () => ({ scroll: 3 }) };
  } });
  vi.stubGlobal("fetch", fetchMock({ 30: "demo.board" }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  expect(seen).toHaveLength(1);
  expect(seen[0]!.action).toEqual({ id: 30, name: "Action 30", tag: "demo.board", params: { report: 7 } });
  expect(seen[0]!.restored).toBeNull();
  expect(root.querySelector("[data-screen=board]")?.textContent).toBe("report 7");
  app.dispose(); app.dispose();
  expect(dispose).toHaveBeenCalledTimes(1);
});

/// (partial: a tag the server sent that the bundle does not register fails by name and mounts nothing)
test("an unregistered client action tag fails by name and mounts nothing", async () => {
  history.replaceState(null, "", "/erp/action-31?menu=11");
  vi.stubGlobal("fetch", fetchMock({ 31: "demo.ghost" }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await expect(app.start()).rejects.toMatchObject({
    kind: "unsupported",
    message: "Client action 'demo.ghost' is declared by the server but not registered by this page. Reload the page."
  });
  expect(root.querySelector("[data-screen]")).toBeNull();
});

/// (partial: a report opens the existing source form through its authorized read)
test("a client action requests the existing source record using the returned form", async () => {
  history.replaceState(null, "", "/erp/action-30?menu=10");
  let screen: ClientActionEnvironment | undefined;
  registerClientAction("demo.source_form", { create: async environment => {
    screen = environment;
    return { dispose() {} };
  } });
  const baseFetch = fetchMock({ 30: "demo.source_form" });
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_normalize_action_result")) return json({ wire_major: 1, result: {
      kind: "window", name: "Source", model: "account.move", res_id: 42,
      views: [{ id: null, type: "form" }], context: {}
    } });
    if (path.endsWith("account.move/get_views")) return json({
      wire_major: 1, registry_version: "r1", description_version: "d1", model: "account.move", notices: [],
      fields: { "account.move": { name: { type: "char", label: "Number", required: false, readonly: true, selection: null, relation: null, relation_field: null } } },
      views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [
        { kind: "form", record_mode: "record", children: [1] },
        { kind: "field", field: { model: "account.move", name: "name" }, widget: "char", label: "Number", force_save: false, options: {}, modifiers: { readonly: null, required: null, invisible: null } }
      ] }]
    });
    if (path.endsWith("account.move/web_read")) {
      expect(JSON.parse(String(init?.body)).ids).toEqual([42]);
      return json([{ id: 42, name: "INV/42" }]);
    }
    return baseFetch(path, init);
  }));
  const createElement = document.createElement.bind(document);
  vi.spyOn(document, "createElement").mockImplementation((tag: string, options?: ElementCreationOptions) => {
    const node = createElement(tag, options);
    if (node instanceof HTMLDialogElement) {
      node.showModal = () => { node.open = true; };
      node.close = () => { node.open = false; };
    }
    return node;
  });
  app = new HumanApp(host(), bootstrap);
  await app.start();
  await screen!.open({ type: "ir.actions.act_window" });
  expect(document.querySelector("dialog")?.textContent).toContain("Source");
  expect(location.search).toBe("?menu=10");
});

test("a record opened in the page from a client action keeps a link back to that screen", async () => {
  history.replaceState(null, "", "/erp/action-30?menu=10");
  let screen: ClientActionEnvironment | undefined;
  let mounts = 0;
  registerClientAction("demo.origin_link", { create: async environment => {
    screen = environment; mounts++;
    return { dispose() {} };
  } });
  const baseFetch = fetchMock({ 30: "demo.origin_link" });
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_normalize_action_result")) return json({ wire_major: 1, result: {
      kind: "current_window", name: "Journal entry", model: "account.move", res_id: 42,
      views: [{ id: null, type: "form" }], domain: [["id", "=", 42]], context: {}
    } });
    if (path.endsWith("account.move/get_views")) return json({
      wire_major: 1, registry_version: "r1", description_version: "d1", model: "account.move", notices: [],
      fields: { "account.move": { name: { type: "char", label: "Number", required: false, readonly: true, selection: null, relation: null, relation_field: null } } },
      views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [
        { kind: "form", record_mode: "record", children: [1] },
        { kind: "field", field: { model: "account.move", name: "name" }, widget: "char", label: "Number", force_save: false, options: {}, modifiers: { readonly: null, required: null, invisible: null } }
      ] }]
    });
    if (path.endsWith("account.move/web_read")) return json([{ id: 42, name: "INV/42" }]);
    return baseFetch(path, init);
  }));
  app = new HumanApp(host(), bootstrap);
  await app.start();
  expect(mounts).toBe(1);
  await screen!.open({ type: "ir.actions.act_window" });
  await vi.waitFor(() => expect(new URL(location.href).searchParams.get("id")).toBe("42"));
  const crumb = await vi.waitFor(() => {
    const link = document.querySelector<HTMLButtonElement>(".breadcrumb-origin");
    expect(link).not.toBeNull();
    return link!;
  });
  // The link names the screen the user came from and sits before the record's own title.
  expect(crumb.textContent).toBe("Action 30");
  expect(crumb.parentElement?.querySelector(".breadcrumb-current")?.textContent).toBe("INV/42");
  crumb.click();
  await vi.waitFor(() => expect(mounts).toBe(2));
  expect(new URL(location.href).searchParams.get("id")).toBeNull();
  expect(new URL(location.href).pathname).toBe("/erp/action-30");
});

test("Discuss opens from the app menu without realtime and uses the active company for its action and conversations", async () => {
  history.replaceState(null, "", "/erp/action-45?menu=12");
  const requested: Array<{ path: string; params: Record<string, unknown> }> = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const params = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
    requested.push({ path, params });
    if (path.endsWith("erp_load_web_menus")) return json({
      ...menus, roots: [12], nodes: [{ module: "mail", id: 12, parent_id: null, label: "Discuss", action_id: 45, action_kind: "client", children: [], icon: null }]
    });
    if (path.endsWith("erp_load_web_client_action")) return json({
      wire_major: 1, result: { kind: "client", id: 45, name: "Discuss", tag: "mail.discuss", params: {} }
    });
    if (path.endsWith("conversation_search")) return json({ items: [], next: null });
    return json([]);
  }));
  const root = host();
  app = new HumanApp(root, {
    ...bootstrap, companies: [...bootstrap.companies, { id: 2, name: "Jakarta" }],
    allowed_company_ids: [1, 2], active_company_ids: [2], current_company_id: 2,
  });
  await app.start();
  const workspace = root.querySelector<HTMLElement>("[data-client-content] .discuss-workspace");
  expect(workspace?.hidden).toBe(false);
  expect(root.querySelector<HTMLElement>("[data-control-panel]")?.hidden).toBe(true);
  expect(workspace?.querySelector("h1")?.textContent).toBe("Discuss");
  await vi.waitFor(() => expect(workspace?.textContent).toContain("No conversations found."));
  for (const method of ["erp_load_web_client_action", "conversation_search"]) {
    expect(requested.find(request => request.path.endsWith(method))?.params["context"]).toMatchObject({ allowed_company_ids: [2] });
  }
  app.dispose();
  expect(document.querySelector(".discuss-workspace")).toBeNull();
});

test("expanding quick chat navigates to the authorized Discuss menu and keeps the same panel when returning home", async () => {
  history.replaceState(null, "", "/erp");
  const discussMenu = { module: "mail", id: 12, parent_id: null, label: "Discuss", action_id: 45, action_kind: "client", children: [], icon: null };
  vi.stubGlobal("WebSocket", undefined);
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    if (path.endsWith("erp_load_web_menus")) return json({ ...menus, roots: [12], nodes: [discussMenu] });
    if (path.endsWith("erp_load_web_client_action")) return json({
      wire_major: 1, result: { kind: "client", id: 45, name: "Discuss", tag: "mail.discuss", params: {} }
    });
    if (path.endsWith("conversation_search")) return json({ items: [], next: null });
    return json([]);
  }));
  const root = host();
  app = new HumanApp(root, { ...bootstrap, thread_reconnect_ms: [100, 800] });
  await app.start();
  root.querySelector<HTMLButtonElement>('[data-client-header] button[aria-label="Discuss"]')!.click();
  const panel = root.querySelector<HTMLElement>("[data-client-header] .discuss-panel")!;
  expect(panel.hidden).toBe(false);
  panel.querySelector<HTMLInputElement>('[aria-label="Search conversations"]')!.value = "Purchasing";
  panel.querySelector<HTMLButtonElement>('button[aria-label="Open workspace"]')!.click();
  await vi.waitFor(() => expect(root.querySelector("[data-client-content] .discuss-workspace")).toBe(panel));
  const trigger = root.querySelector<HTMLButtonElement>(".discuss-trigger")!;
  expect(trigger.hidden).toBe(true);
  expect(location.pathname).toBe("/erp/action-45");
  expect(new URLSearchParams(location.search).get("menu")).toBe("12");
  expect(panel.querySelector<HTMLInputElement>('[aria-label="Search conversations"]')!.value).toBe("Purchasing");
  root.querySelector<HTMLButtonElement>("button.brand")!.click();
  await vi.waitFor(() => expect(root.querySelector("[data-client-header] .discuss-panel")).toBe(panel));
  expect(panel.hidden).toBe(true);
  expect(trigger.hidden).toBe(false);
  expect(panel.querySelector("h2")?.textContent).toBe("Discuss");
  expect(panel.querySelector("h1")).toBeNull();
});

test("an editable client action guards navigation while busy and saves a draft before leaving", async () => {
  const createElement = document.createElement.bind(document);
  vi.spyOn(document, "createElement").mockImplementation((tag, options) => {
    const node = createElement(tag, options);
    if (node instanceof HTMLDialogElement) {
      node.showModal = () => { node.open = true; };
      node.close = () => { node.open = false; node.dispatchEvent(new Event("close")); };
    }
    return node;
  });
  history.replaceState(null, "", "/erp/action-30?menu=10");
  let env!: ClientActionEnvironment;
  let busy = true;
  let dirty = true;
  const edits = {
    busy: () => busy, dirty: () => dirty,
    save: vi.fn(async () => { dirty = false; }), discard: vi.fn(() => { dirty = false; }),
  };
  registerClientAction("demo.workflow_editor", { async create(context) {
    env = context;
    return { edits, dispose() {} };
  } });
  vi.stubGlobal("fetch", fetchMock({ 30: "demo.workflow_editor" }));
  app = new HumanApp(host(), bootstrap);
  await app.start();
  await env.navigate!({});
  expect(location.pathname).toBe("/erp/action-30");
  busy = false;
  const choose = async (name: string) => {
    const navigation = env.navigate!({});
    const dialog = await vi.waitFor(() => {
      const node = [...document.querySelectorAll("dialog")].find(d => d.textContent?.includes("Unsaved changes"));
      expect(node, document.querySelector("dialog")?.textContent ?? document.querySelector("[data-client-notice]")?.textContent ?? "No dialog").toBeDefined();
      return node!;
    });
    [...dialog.querySelectorAll("button")].find(b => b.textContent === name)!.click();
    await navigation;
  };
  await choose("Cancel");
  expect(location.pathname).toBe("/erp/action-30");
  expect(dirty).toBe(true);
  await choose("Save");
  expect(location.pathname).toBe("/erp");
  expect(edits.save).toHaveBeenCalledOnce();
  expect(edits.discard).not.toHaveBeenCalled();
});

const approvalsMenu = { module: "approvals", id: 12, parent_id: null, label: "Approvals", action_id: 61, action_kind: "window", children: [], icon: null };
const approvalsModel = "approval.request.view";
const requestAction = { wire_major: 1, id: 61, name: "Approval requests", model: approvalsModel, target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null };
const requestView = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: approvalsModel, notices: [],
  fields: { [approvalsModel]: { name: { type: "char", label: "Request", required: false, readonly: true, selection: null, relation: null, relation_field: null } } },
  views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "field", field: { model: approvalsModel, name: "name" }, widget: "char", label: "Request", force_save: false, options: {}, modifiers: { readonly: null, required: null, invisible: null } }
  ] }]
};

test.each(["first load", "browser Back"])("a retired Approvals record link becomes a native route on %s", async visit => {
  history.replaceState({ erpIndex: 21 }, "", `/erp/action-60?${visit === "first load" ? "" : "menu=12&"}id=23&view_type=form&list_tab=reviewed:50`);
  if (visit === "browser Back") history.pushState({ erpIndex: 22 }, "", "/erp");
  const entryCount = history.length;
  const calls: { path: string; args: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const args = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
    calls.push({ path, args });
    if (path.endsWith("erp_load_web_menus")) return json({ ...menus, roots: [12], nodes: [approvalsMenu] });
    if (path.endsWith("erp_load_web_action_views")) {
      if ((args["ids"] as number[])[0] !== 60) return json({ action: requestAction, views: requestView });
      expect(args["bookmark"]).toBe(true);
      return json(clientAction(60, "approvals.requests"));
    }
    if (path.endsWith("erp_load_web_action_ref")) return json(requestAction);
    if (path.endsWith(`${approvalsModel}/web_read`)) return json([{ id: 23, name: "Request #23" }]);
    if (path.endsWith("res.users/web_read")) return json([{ id: 2, action_id: false }]);
    return json([]);
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  if (visit === "browser Back") history.back();
  await vi.waitFor(() => expect(root.querySelector("[data-client-content]")?.textContent).toContain("Request #23"));
  const url = new URL(location.href);
  expect(url.pathname).toBe("/erp/action-61");
  expect(url.searchParams.get("menu")).toBe(visit === "first load" ? null : "12");
  expect(url.searchParams.get("id")).toBe("23");
  expect(url.searchParams.get("view_type")).toBe("form");
  expect(url.searchParams.get("search_filters")).toBe('["is_reviewed"]');
  expect(url.searchParams.has("list_tab")).toBe(false);
  expect(history.state).toEqual({ erpIndex: 21 });
  expect(history.length).toBe(entryCount);
  expect(calls.find(call => call.path.endsWith("erp_load_web_action_ref"))?.args["xml_id"]).toBe("approvals.requests_window");
  expect(calls.find(call => call.path.endsWith(`${approvalsModel}/web_read`))?.args["ids"]).toEqual([23]);
});

test("a denied native menu action preserves the refusal without probing a legacy client action", async () => {
  history.replaceState(null, "", "/erp/action-61?menu=12");
  const fetch = vi.fn(async (path: string) => {
    if (path.endsWith("erp_load_web_menus")) return json({ ...menus, roots: [12], nodes: [approvalsMenu] });
    if (path.endsWith("erp_load_web_action_views")) return new Response(JSON.stringify({ message: "Access denied." }), { status: 403 });
    return json([]);
  });
  vi.stubGlobal("fetch", fetch);
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  expect(root.querySelector("[data-client-content]")?.textContent).toContain("This screen is not open to your account.");
  expect(fetch.mock.calls.some(([path]) => path.endsWith("erp_load_web_client_action"))).toBe(false);
});

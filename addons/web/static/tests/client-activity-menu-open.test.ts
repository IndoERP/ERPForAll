/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";

function actionMetadata(path: string, action: unknown, views: unknown): unknown {
  return path.endsWith("erp_load_web_action_views") ? { action, views } : action;
}

const bootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Arkana" }],
  current_company_id: 1,
  allowed_company_ids: [1],
  active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};

const noteDescription = {
  wire_major: 1,
  registry_version: "r1",
  description_version: "d1",
  model: "test.note",
  fields: {
    "test.note": {
      name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null }
    }
  },
  notices: [],
  views: [
    {
      identity: { kind: "stored", id: 1 },
      type: "form",
      schema_major: 1,
      renderer: "web.form",
      root: 0,
      nodes: [
        { kind: "form", record_mode: "record", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    },
    {
      identity: { kind: "stored", id: 2 },
      type: "list",
      schema_major: 1,
      renderer: "web.list",
      root: 0,
      nodes: [
        { kind: "list", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    }
  ]
};

let app: HumanApp | undefined;
const showModalDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const closeDialogDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
});

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

afterEach(() => {
  app?.dispose();
  app = undefined;
  if (showModalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", showModalDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (closeDialogDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDialogDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  vi.restoreAllMocks();
  document.body.replaceChildren();
  document.body.style.overflow = "";
  delete document.documentElement.dataset["officeTheme"];
  delete document.documentElement.dataset["mobile"];
  vi.unstubAllGlobals();
  history.replaceState(null, "", "/");
});

function host(): HTMLElement {
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    root.append(node);
  }
  document.body.append(root);
  return root;
}

/**
  What the server refuses for the two calls the open makes. `search_read`: crates/erp-orm/src/rpc/core.rs SEARCH_READ_PARAMS (optional
 *  domain, fields, offset, limit, order, load); `erp_normalize_action_result`: addons/web/src/metadata.rs (required `wire_major` and `result`).
 *  In both, crates/erp-rpc/src/binding.rs refuses an unknown key (422) and reserves `context`. 
 */
function refuseWhatTheServerRefuses(method: string, body: Record<string, unknown>): void {
  const params = method === "search_read" ? ["domain", "fields", "offset", "limit", "order", "load"] : ["wire_major", "result"];
  const unknown = Object.keys(body).find(key => key !== "context" && !params.includes(key));
  if (unknown) throw new Error(`422: got an unexpected keyword argument '${unknown}'`);
  const unsigned = (value: unknown): boolean => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  if (method === "search_read") {
    if (body["domain"] != null && !Array.isArray(body["domain"])) throw new Error("422: domain must be a list");
    if (body["fields"] != null && !(Array.isArray(body["fields"]) && body["fields"].every(name => typeof name === "string"))) throw new Error("422: fields must be a list of strings");
    for (const key of ["offset", "limit"]) if (body[key] != null && !unsigned(body[key])) throw new Error(`422: ${key} must be a non-negative integer`);
  } else {
    if (!unsigned(body["wire_major"])) throw new Error("422: wire_major must be a non-negative integer");
    if (typeof body["result"] !== "object" || body["result"] === null || Array.isArray(body["result"])) throw new Error("422: result must be an object");
  }
}

function mockNoteFetch(log: string[], options: { rows?: number; normalized?: unknown } = {}) {
  return vi.fn(async (path: string, init?: RequestInit) => {
    log.push(path.split("/").slice(-2).join("/"));
    if (path.endsWith("erp_load_web_menus")) {
      return new Response(JSON.stringify({
        wire_major: 1,
        registry_version: "r1",
        roots: [10],
        nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("ir.actions.act_window/search_read")) {
      refuseWhatTheServerRefuses("search_read", JSON.parse(String(init?.body ?? "{}")));
      return new Response(JSON.stringify(Array.from({ length: options.rows ?? 1 }, (_, i) => ({ id: 30 + i }))), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("erp_normalize_action_result")) {
      const body = JSON.parse(String(init?.body));
      refuseWhatTheServerRefuses("erp_normalize_action_result", body);
      if (options.normalized) return new Response(JSON.stringify({ wire_major: 1, result: options.normalized }), { headers: { "Content-Type": "application/json" } });
      return new Response(JSON.stringify({ wire_major: 1, result: { kind: "current_window", name: body.result.name, model: "test.note",
        views: [{ id: null, type: "list" }, { id: null, type: "form" }], domain: body.result.domain, context: {} } }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) {
      const asked = JSON.parse(String(init?.body ?? "{}")).ids?.[0] ?? 20;
      return new Response(JSON.stringify(actionMetadata(path, {
        wire_major: 1,
        id: asked,
        name: "Notes",
        model: "test.note",
        target: "current",
        views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
        domain: null,
        context: null
      }, noteDescription)), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("get_views")) {
      return new Response(JSON.stringify(noteDescription), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_search_read")) {
      return new Response(JSON.stringify({
        length: 2,
        records: [
          { id: 5, name: "Meeting Agenda" },
          { id: 6, name: "Project Roadmap" }
        ]
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_read")) {
      const body = init?.body ? JSON.parse(String(init.body)) : {};
      const id = body.ids?.[0] ?? 5;
      const name = id === 6 ? "Project Roadmap" : "Meeting Agenda";
      return new Response(JSON.stringify([{ id, name }]), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("onchange")) {
      return new Response(JSON.stringify({ value: {} }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_save")) {
      const body = init?.body ? JSON.parse(String(init.body)) : {};
      
      return new Response(JSON.stringify([{ id: 5, ...body.vals }]), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify([]), { headers: { "Content-Type": "application/json" } });
  });
}


const activityAction = { type: "ir.actions.act_window", name: "Contact", res_model: "test.note", target: "current",
  views: [[false, "list"], [false, "form"]], context: {}, domain: [["id", "in", [5, 6]]] };

/// (partial: the app anchors a menu-opened list on My Activities, admits it through the normalizer and focuses the list)
test("openActivities anchors on My Activities, normalizes, opens the list and focuses its first control", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log: string[] = [];
  vi.stubGlobal("fetch", mockNoteFetch(log));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const open = (app as unknown as { openActivities(action: unknown): Promise<void> }).openActivities.bind(app);
  await open(activityAction);
  expect(log).toContain("ir.actions.act_window/search_read");
  expect(log).toContain("mail.activity/erp_normalize_action_result");
  expect(location.search).toContain("method_action=");
  expect(location.pathname).toBe("/erp/action-30");
  expect(root.textContent).toContain("Contact");
  expect(root.querySelector("[data-client-content]")!.contains(document.activeElement)).toBe(true);
  // the first control of the list itself, not of the control panel above it
  expect(root.querySelector(".list-table")!.contains(document.activeElement)).toBe(true);
  // "View all activities": the persisted action itself, with no method action
  await open(null);
  expect(location.pathname).toBe("/erp/action-30");
  expect(location.search).not.toContain("method_action");
});

type Opener = { openActivities(action: unknown): Promise<void>; navigate(...args: unknown[]): Promise<void>; methodActions: { entries: Map<string, unknown> } };
async function started(options: { rows?: number; normalized?: unknown } = {}): Promise<Opener> {
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", mockNoteFetch([], options));
  app = new HumanApp(host(), bootstrap);
  await app.start();
  return app as unknown as Opener;
}

/// (partial: a missing My Activities action is refused with a message a user can act on)
test("an anchor lookup with 0 rows is refused loudly", async () => {
  const message = "The My Activities list is missing. Ask your administrator to restore it.";
  const opener = await started({ rows: 0 });
  await expect(opener.openActivities(null)).rejects.toThrow(message);
  await expect(opener.openActivities(activityAction)).rejects.toThrow(message);
});

/// (partial: a duplicated My Activities action is refused with a message a user can act on)
test("an anchor lookup with 2 rows is refused loudly", async () => {
  const message = "The My Activities list is set up twice. Ask your administrator to restore it.";
  const opener = await started({ rows: 2 });
  await expect(opener.openActivities(null)).rejects.toThrow(message);
  await expect(opener.openActivities(activityAction)).rejects.toThrow(message);
});

/// (partial: a normalizer answer that is not the current-window form is refused)
test("a normalizer answer other than a current window is refused", async () => {
  const opener = await started({ normalized: { kind: "none" } });
  await expect(opener.openActivities(activityAction)).rejects.toThrow("did not open as a list");
  expect(opener.methodActions.entries.size).toBe(0);
});

/// (partial: a navigation that throws leaves no method-action entry behind)
test("a failed navigation removes its method-action entry", async () => {
  const opener = await started();
  vi.spyOn(opener as unknown as { navigate: () => Promise<void> }, "navigate").mockRejectedValue(new Error("boom"));
  await expect(opener.openActivities(activityAction)).rejects.toThrow("boom");
  expect(opener.methodActions.entries.size).toBe(0);
});

/// (partial: a navigation that did not happen drops its entry and does not move focus)
test("a navigation that returns without navigating drops its entry and keeps focus", async () => {
  const opener = await started();
  const before = document.activeElement;
  vi.spyOn(opener as unknown as { navigate: () => Promise<void> }, "navigate").mockResolvedValue(undefined);
  await opener.openActivities(activityAction);
  expect(opener.methodActions.entries.size).toBe(0);
  expect(document.activeElement).toBe(before);
});

/// (partial: the user message of a missing anchor names no model, domain or id; the detail goes to the console)
test("the anchor refusal keeps technical detail out of the user message", async () => {
  const detail = vi.spyOn(console, "error").mockImplementation(() => {});
  const opener = await started({ rows: 0 });
  const error = await opener.openActivities(null).catch((reason: Error) => reason);
  expect((error as Error).message).not.toMatch(/mail\.activity|user_id|mail_activity_action_my/);
  expect(detail).toHaveBeenCalledWith(expect.stringContaining("mail.mail_activity_action_my"));
});

/// (partial: a normalized list of another model than the action's is refused)
test("a normalized model that differs from the action's is refused", async () => {
  const opener = await started({ normalized: { kind: "current_window", name: "Contact", model: "other.model",
    views: [{ id: null, type: "list" }, { id: null, type: "form" }], domain: [["id", "in", [5, 6]]], context: {} } });
  await expect(opener.openActivities(activityAction)).rejects.toThrow("requested model");
  expect(opener.methodActions.entries.size).toBe(0);
});

/// (partial: a list without a focusable control leaves focus on the page, with no error)
test("a list with no focusable control opens without moving focus into it", async () => {
  const opener = await started();
  const content = (opener as unknown as { content: HTMLElement }).content;
  const original = content.querySelector.bind(content) as (selector: string) => Element | null;
  vi.spyOn(content, "querySelector").mockImplementation(((selector: string) => selector === ".list-table" ? null : original(selector)) as typeof content.querySelector);
  await opener.openActivities(activityAction);
  // no error, and no control inside the list table was focused
  expect(document.querySelector(".list-table")!.contains(document.activeElement)).toBe(false);
});

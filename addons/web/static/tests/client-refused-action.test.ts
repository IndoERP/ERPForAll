/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import { refusedActionPages, registerRefusedActionPage } from "../src/client/refused-action.js";

const SERVER_TEXT = "Secret payroll action is restricted to group X.";

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

const field = { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" };
const description = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note",
  fields: { "test.note": { name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null } } },
  notices: [],
  views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field] }
  ]
};

const json = (value: unknown, status = 200): Response => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });

/** Menu 10 opens the allowed action 20; menu 12 opens action 22, which the server answers with `refusal`. */
function serve(refusal: { status: number; body: unknown } = { status: 403, body: { name: "erp.exceptions.AccessError", message: SERVER_TEXT } }) {
  return vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_load_web_menus")) {
      return json({ wire_major: 1, registry_version: "r1", roots: [10, 12], nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
        { module: null, id: 12, parent_id: null, label: "Payroll", action_id: 22, children: [], icon: null }
      ] });
    }
    if (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) {
      const id = JSON.parse(String(init?.body ?? "{}")).ids?.[0];
      if (id === 22) return json(refusal.body, refusal.status);
      const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
      return json(path.endsWith("erp_load_web_action_views") ? { action, views: description } : action);
    }
    if (path.endsWith("get_views")) return json(description);
    if (path.endsWith("web_search_read")) return json({ length: 1, records: [{ id: 5, name: "Meeting Agenda" }] });
    return json([]);
  });
}

let app: HumanApp | undefined;
const showModal = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const close = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  configureClientTemplates([{
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
  }]);
});

afterEach(() => {
  app?.dispose();
  app = undefined;
  for (const [key] of refusedActionPages.entries()) refusedActionPages.remove(key);
  if (showModal) Object.defineProperty(HTMLDialogElement.prototype, "showModal", showModal);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (close) Object.defineProperty(HTMLDialogElement.prototype, "close", close);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  document.body.replaceChildren();
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

const content = (root: HTMLElement): HTMLElement => root.querySelector<HTMLElement>("[data-client-content]")!;

test("a refused action in the address shows the shell and the fixed page, never the server's text", async () => {
  history.replaceState(null, "", "/erp/action-22?menu=12&cids=1");
  vi.stubGlobal("fetch", serve());
  const root = host();
  app = new HumanApp(root, bootstrap);
  const length = history.length;

  await app.start();

  expect(content(root).textContent).toContain("You do not have access to this page.");
  expect(root.textContent).not.toContain(SERVER_TEXT);
  expect(document.querySelector("dialog.client-error-dialog")).toBeNull();
  // the shell stays: header and menus are drawn, and no menu entry is current
  expect(root.querySelector("[data-client-header]")!.childElementCount).toBeGreaterThan(0);
  expect(root.querySelector("[data-menu-id]")).not.toBeNull();
  expect(root.querySelector("[data-menu-id][aria-current]")).toBeNull();
  // the refused address is replaced by home with the same companies, not pushed
  expect(location.pathname).toBe("/erp");
  expect(new URLSearchParams(location.search).get("cids")).toBe("1");
  expect(history.length).toBe(length);
});

test("Go to home opens the app grid", async () => {
  history.replaceState(null, "", "/erp/action-22");
  vi.stubGlobal("fetch", serve());
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  content(root).querySelector<HTMLButtonElement>(".refused-action-home")!.click();

  await vi.waitFor(() => expect(content(root).querySelector(".home-menu-container")).not.toBeNull());
  expect(content(root).textContent).not.toContain("You do not have access to this page.");
});

test("a refusal after a menu click replaces the previous page and keeps Back on it", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=10");
  vi.stubGlobal("fetch", serve());
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  expect(content(root).textContent).toContain("Meeting Agenda");
  const length = history.length;

  root.querySelector<HTMLElement>('[data-menu-id="12"]')!.click();

  await vi.waitFor(() => expect(content(root).textContent).toContain("You do not have access to this page."));
  expect(content(root).textContent).not.toContain("Meeting Agenda");
  expect(root.textContent).not.toContain(SERVER_TEXT);
  expect(document.querySelector("dialog.client-error-dialog")).toBeNull();
  // one new entry, the home address; the refused one is never recorded
  expect(history.length).toBe(length + 1);
  expect(location.pathname).toBe("/erp");
});

test("a server error on the action still opens the error dialog", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=10");
  vi.stubGlobal("fetch", serve({ status: 500, body: { message: "Boom" } }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  root.querySelector<HTMLElement>('[data-menu-id="12"]')!.click();

  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")).not.toBeNull());
  expect(content(root).textContent).not.toContain("You do not have access to this page.");
  expect(content(root).textContent).toContain("Meeting Agenda");
});

test("a registered page replaces the body and is disposed by the next navigation", async () => {
  const dispose = vi.fn();
  const seen: string[][] = [];
  registerRefusedActionPage("test.self_service", {
    create(environment) {
      seen.push(Object.keys(environment).sort());
      const element = document.createElement("p");
      element.className = "self-service";
      element.textContent = "Open My Payslips instead.";
      return { element, dispose };
    }
  });
  history.replaceState(null, "", "/erp/action-22");
  vi.stubGlobal("fetch", serve());
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  expect(content(root).querySelector(".self-service")?.textContent).toBe("Open My Payslips instead.");
  expect(content(root).textContent).not.toContain("You do not have access to this page.");
  // the page gets services, never the refused action
  expect(seen).toEqual([["navigate", "rpc"]]);

  root.querySelector<HTMLElement>('[data-menu-id="10"]')!.click();
  await vi.waitFor(() => expect(content(root).textContent).toContain("Meeting Agenda"));
  expect(dispose).toHaveBeenCalledTimes(1);
});

test("a registered page that throws is reported and the fixed page is shown", async () => {
  registerRefusedActionPage("test.broken", { create() { throw new Error("broken page"); } });
  history.replaceState(null, "", "/erp/action-22");
  vi.stubGlobal("fetch", serve());
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  expect(content(root).textContent).toContain("You do not have access to this page.");
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("broken page"));
});

test("a second refused-action page is refused", () => {
  const page = { create: () => ({ element: document.createElement("p"), dispose() {} }) };
  registerRefusedActionPage("test.first", page);
  expect(() => registerRefusedActionPage("test.second", page)).toThrow("already registered: test.first");
});

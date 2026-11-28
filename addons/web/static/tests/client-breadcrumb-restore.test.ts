/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import { NAVIGATION_KEY } from "../src/client/navigation-store.js";
import { clearProtectedTabState } from "../src/client/session-boundary.js";

// The breadcrumb trail of each history entry is kept in the tab's session storage: a reload and the browser's Back
// and Forward bring it back, what is read back is checked entry by entry, and sign-out erases it.

const bootstrap = (uid = 2) => ({
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
});

const modifiers = { readonly: null, required: null, invisible: null };
const char = (model: string) => ({ kind: "field", force_save: false, field: { model, name: "name" }, widget: "char", options: {}, modifiers, label: "Name" });
const link = (name: string, label: string, model: string) => ({ kind: "field", force_save: false, field: { model, name }, widget: "many2one", options: {}, modifiers, label });
const relation = (label: string, target: string) => ({ type: "many2one", label, required: false, readonly: true, relation: target, relation_field: null, selection: null });
const nameField = { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null };
const details = { size: "normal", circle: false, classes: [], debounce_ms: 0, block_ui: false };

const noteViews = {
  wire_major: 1, registry_version: "r1", description_version: "n1", model: "test.note", notices: [],
  fields: { "test.note": { name: nameField, company_id: relation("Company", "res.company") } },
  views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
      nodes: [{ kind: "form", record_mode: "record", children: [3, 1, 2] }, char("test.note"), link("company_id", "Company", "test.note"),
        { kind: "header", children: [4, 5] },
        { kind: "button", name: "action_view_companies", button_type: "object", label: "Note companies", variant: "secondary", confirm: null, modifiers, details },
        { kind: "button", name: "action_view_company", button_type: "object", label: "Open company", variant: "secondary", confirm: null, modifiers, details }] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, char("test.note")] }
  ]
};
const companyViews = {
  wire_major: 1, registry_version: "r1", description_version: "c1", model: "res.company", notices: [],
  fields: { "res.company": { name: nameField, note_id: relation("Note", "test.note") } },
  views: [
    { identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
      nodes: [{ kind: "form", record_mode: "record", children: [1, 2] }, char("res.company"), link("note_id", "Note", "res.company")] },
    { identity: { kind: "stored", id: 4 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, char("res.company")] }
  ]
};
const notesAction = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
const companiesAction = { wire_major: 1, id: 21, name: "Companies", model: "res.company", target: "current", views: [{ id: 4, type: "list" }, { id: 3, type: "form" }], domain: null, context: null };
const notes: Record<number, Record<string, unknown>> = {
  5: { id: 5, name: "Source record", company_id: { id: 9, display_name: "Company record" } },
  6: { id: 6, name: "Parent record", company_id: false }
};

let app: HumanApp | undefined;
let root: HTMLElement;
/** Records the server refuses to read (`web_read` answers 403). */
let refused: Set<number>;
/** Whether the server refuses to admit a related list again. */
let refuseList: boolean;
/** Records whose read fails on the server (`web_read` answers 500). */
let failing: Set<number>;
/** The request (`model/method`) the server answers with 401: the session ended. */
let expireOn: string | undefined;
const calls: string[] = [];
/** The results sent to the server's admission, in order. */
const admitted: unknown[] = [];
const showModal = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const closeDialog = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  refused = new Set();
  refuseList = false;
  failing = new Set();
  expireOn = undefined;
  calls.length = 0;
  admitted.length = 0;
  configureClientTemplates([{
    name: "web.HumanControlPanel",
    render: () => {
      const panel = document.createElement("section");
      panel.setAttribute("data-control-panel", "");
      for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
        const node = document.createElement("div");
        node.setAttribute(`data-control-${key}`, "");
        panel.append(node);
      }
      return panel;
    }
  }]);
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body ?? "{}")) as Record<string, unknown>;
    const call = path.split("/").slice(-2).join("/");
    calls.push(call);
    if (call === expireOn) return new Response(JSON.stringify({ name: "erp.http.session.SessionExpiredException", message: "Session expired." }), { status: 401, headers: { "Content-Type": "application/json" } });
    let value: unknown = [];
    if (call.endsWith("erp_load_web_menus")) value = { wire_major: 1, registry_version: "r1", roots: [10, 11], nodes: [
      { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
      { module: null, id: 11, parent_id: null, label: "Companies", action_id: 21, children: [], icon: null }] };
    else if (call.endsWith("erp_resolve_record_action")) value = body["model"] === "res.company"
      ? { menu: 11, any_record: true, action: companiesAction, views: companyViews }
      : { menu: 10, any_record: true, action: notesAction, views: noteViews };
    else if (call.endsWith("erp_load_web_action_views")) value = (body["ids"] as number[])[0] === 21 ? { action: companiesAction, views: companyViews } : { action: notesAction, views: noteViews };
    else if (call === "test.note/action_view_companies") value = { type: "ir.actions.act_window", name: "Note companies", res_model: "res.company", target: "current", views: [[false, "list"], [false, "form"]], domain: [["id", "=", 9]] };
    else if (call === "test.note/action_view_company") value = { type: "ir.actions.act_window", name: "Company", res_model: "res.company", target: "current", res_id: 9, views: [[3, "form"]], context: { create: false } };
    else if (call.endsWith("erp_normalize_action_result")) {
      admitted.push(body["result"]);
      const one = (body["result"] as { res_id?: number }).res_id;
      if (one !== undefined) {
        // The server's check of one record: it must exist and the user must read it.
        if (refused.has(one)) return new Response(JSON.stringify({ name: "erp.exceptions.AccessError", message: "The res.company record to open does not exist or you may not read it." }), { status: 403, headers: { "Content-Type": "application/json" } });
        const result = body["result"] as { name: string; res_model: string; views: [number | false, string][] };
        value = { wire_major: 1, result: { kind: "current_window", name: result.name, model: result.res_model, views: result.views.map(([id, type]) => ({ id: id === false ? null : id, type })), domain: [["id", "=", one]], res_id: one, context: { create: false } } };
        return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
      }
      if (refuseList) return new Response(JSON.stringify({ name: "erp.exceptions.AccessError", message: "You may not read these records." }), { status: 403, headers: { "Content-Type": "application/json" } });
      // The server's admission of a window result, reduced to what these cases send.
      const result = body["result"] as { name: string; res_model: string; views: [number | false, string][]; domain?: unknown[] };
      value = { wire_major: 1, result: { kind: "current_window", name: result.name, model: result.res_model, views: result.views.map(([id, type]) => ({ id: id === false ? null : id, type })), domain: result.domain ?? [], context: {} } };
    }
    else if (call === "res.company/get_views") value = companyViews;
    else if (call === "res.company/web_search_read") value = { length: 1, records: [{ id: 9, name: "Company record" }] };
    else if (call === "test.note/web_search_read") value = { length: 2, records: [{ id: 5, name: "Source record" }, { id: 6, name: "Parent record" }] };
    else if (call === "test.note/web_read" || call === "res.company/web_read") {
      const id = (body["ids"] as number[])[0]!;
      if (failing.has(id)) return new Response(JSON.stringify({ name: "erp.exceptions.ServerError", message: "The server failed to read the record." }), { status: 500, headers: { "Content-Type": "application/json" } });
      if (refused.has(id)) return new Response(JSON.stringify({ name: "erp.exceptions.AccessError", message: "You may not read this record." }), { status: 403, headers: { "Content-Type": "application/json" } });
      value = call === "test.note/web_read" ? [notes[id]] : [{ id: 9, name: "Company record", note_id: { id: 6, display_name: "Parent record" } }];
    }
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
});

afterEach(() => {
  app?.dispose();
  app = undefined;
  if (showModal) Object.defineProperty(HTMLDialogElement.prototype, "showModal", showModal);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (closeDialog) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDialog);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  vi.restoreAllMocks();
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  history.replaceState(null, "", "/");
});

/** Mounts the client on the address and history entry the tab has now, as a page load does. */
async function load(uid = 2): Promise<void> {
  app?.dispose();
  document.body.replaceChildren();
  root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    root.append(node);
  }
  document.body.append(root);
  app = new HumanApp(root, bootstrap(uid));
  await app.start();
}

const opened = (name: string) => vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe(name));
const open = (label: string) => root.querySelector<HTMLButtonElement>(`[aria-label="Open ${label}"]`)!.click();
const back = () => root.querySelector<HTMLButtonElement>(".breadcrumb-previous .breadcrumb-back")!.click();
/** The trail as drawn: the older entries (oldest first), the previous entry, the current one. */
const trail = () => {
  const crumbs = root.querySelector("[data-control-breadcrumbs]")!;
  const older = [...crumbs.querySelectorAll(".breadcrumb-dropdown-item")].map(item => item.textContent).reverse();
  const previous = crumbs.querySelector(".breadcrumb-previous .breadcrumb-back")?.textContent;
  return [...older, ...(previous ? [previous] : []), crumbs.querySelector(".breadcrumb-current")?.textContent];
};
const stored = () => JSON.parse(sessionStorage.getItem(NAVIGATION_KEY) ?? "null") as { uid: number; trails: [number, { id: string; title: string; url: string; form: boolean }[]][] } | null;

/** Notes list › Source record › Company record › Parent record: three steps, each on top of the one before. */
async function depthThree(): Promise<void> {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  open("Company: Company record");
  await opened("Company record");
  open("Note: Parent record");
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record", "Parent record"]);
}

test("a reload at depth three restores the trail, and each crumb lands on its own step", async () => {
  await depthThree();
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record", "Parent record"]);
  back();
  await opened("Company record");
  expect(location.pathname).toBe("/erp/action-21");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
  back();
  await opened("Source record");
  expect(new URLSearchParams(location.search).get("id")).toBe("5");
  expect(trail()).toEqual(["Notes", "Source record"]);
  back();
  await vi.waitFor(() => expect(root.querySelector("tbody tr[data-id='5']")).not.toBeNull());
  expect(trail()).toEqual(["Notes"]);
});

test("Back and Forward bring each history entry back with the trail it had", async () => {
  await depthThree();
  history.back();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
  history.back();
  await opened("Source record");
  expect(trail()).toEqual(["Notes", "Source record"]);
  history.forward();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
  history.forward();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record", "Parent record"]);
  // A reload after stepping keeps the entry it is on.
  history.back();
  await opened("Company record");
  await load();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
});

test("a tampered store keeps only the entries this client writes, and never shows another user's trail", async () => {
  await depthThree();
  const data = stored()!;
  const index = (history.state as { erpIndex: number }).erpIndex;
  const entries = data.trails.find(([at]) => at === index)![1];
  expect(entries.map(entry => entry.title)).toEqual(["Notes", "Source record", "Company record", "Parent record"]);
  const tampered = [
    entries[0],
    { ...entries[1], url: "//elsewhere.example/erp/action-20?id=5&view_type=form" },
    { ...entries[1], title: "Bad\u0000title" },
    { ...entries[1], id: "<script>" },
    { ...entries[1], url: "/erp/action-20?id=5&view_type=form&evil=1" },
    { ...entries[1], form: "yes" },
    "not an entry",
    entries[2], entries[3]
  ];
  sessionStorage.setItem(NAVIGATION_KEY, JSON.stringify({ ...data, trails: [[index, tampered]] }));
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Company record", "Parent record"]);

  // Unreadable text and another user's store are dropped whole.
  sessionStorage.setItem(NAVIGATION_KEY, "{not json");
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Parent record"]);
  sessionStorage.setItem(NAVIGATION_KEY, JSON.stringify({ ...data, uid: 7 }));
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Parent record"]);
  expect(stored()!.uid).toBe(2);
});

test("a kept entry the server no longer opens drops out of the trail and the rest stays usable", async () => {
  await depthThree();
  await load();
  await opened("Parent record");
  refused.add(9);
  back();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("You may not read this record."));
  expect(location.pathname).toBe("/erp/action-20");
  expect(trail()).toEqual(["Notes", "Source record", "Parent record"]);
  expect(stored()!.trails.at(-1)![1].map(entry => entry.title)).toEqual(["Notes", "Source record", "Parent record"]);
  document.querySelector<HTMLButtonElement>("dialog.client-error-dialog [data-error-close]")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")).toBeNull());
  back();
  await opened("Source record");
  expect(trail()).toEqual(["Notes", "Source record"]);
});

test("an address opened afresh starts its own trail; sign-out erases the store", async () => {
  await depthThree();
  expect(sessionStorage.getItem(NAVIGATION_KEY)).not.toBeNull();
  // The same address without its history entry (a pasted link) has no trail behind it.
  history.replaceState(null, "", location.pathname + location.search);
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Parent record"]);
  const submitted = vi.spyOn(HTMLFormElement.prototype, "submit").mockImplementation(() => {});
  const logout = root.querySelector<HTMLFormElement>('form[action$="/erp/session/logout"]')!;
  logout.dispatchEvent(new Event("submit", { cancelable: true }));
  await vi.waitFor(() => expect(submitted).toHaveBeenCalled());
  expect(sessionStorage.getItem(NAVIGATION_KEY)).toBeNull();
  // The sign-in page (and an expired session) erase it the same way.
  sessionStorage.setItem(NAVIGATION_KEY, "{}");
  clearProtectedTabState();
  expect(sessionStorage.getItem(NAVIGATION_KEY)).toBeNull();
});

const headerButton = (label: string) => [...root.querySelectorAll<HTMLButtonElement>(".form-header-btn")].find(item => item.textContent?.includes(label))!;
const listed = (id: number) => vi.waitFor(() => expect(root.querySelector(`tbody tr[data-id='${id}']`)).not.toBeNull());
const recovery = () => vi.waitFor(() => expect(root.querySelector("a.method-action-recovery")?.getAttribute("href")).toBe("/erp/action-20"));

/** Notes list › Source record › the related list its button opened. */
async function relatedList(): Promise<string> {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  await vi.waitFor(() => expect(headerButton("Note companies").disabled).toBe(false));
  headerButton("Note companies").click();
  await listed(9);
  expect(trail()).toEqual(["Notes", "Source record", "Note companies"]);
  return new URLSearchParams(location.search).get("method_action")!;
}

test("a related list retains selection and Actions when a refused breadcrumb is removed", async () => {
  await relatedList();
  root.querySelector<HTMLInputElement>(".list-row-select")!.click();
  const actions = root.querySelector<HTMLButtonElement>(".list-data-toggle-btn");
  refused.add(5);
  back();
  await vi.waitFor(() => expect(trail()).toEqual(["Notes", "Note companies"]));
  expect(root.querySelector(".list-data-toggle-btn")).toBe(actions);
  expect(root.querySelector(".selection-count")?.textContent).toBe("1 selected");
});

test("a related list a button opened comes back after a reload, admitted again by the server", async () => {
  const related = await relatedList();
  calls.length = 0;
  await load();
  await listed(9);
  // The kept list went to the server again before it opened.
  expect(calls).toContain("res.company/erp_normalize_action_result");
  expect(new URLSearchParams(location.search).get("method_action")).toBe(related);
  expect(trail()).toEqual(["Notes", "Source record", "Note companies"]);
  root.querySelector<HTMLElement>("tbody tr[data-id='9']")!.click();
  await opened("Company record");
  await load();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Note companies", "Company record"]);
  back();
  await listed(9);
  expect(trail()).toEqual(["Notes", "Source record", "Note companies"]);
  back();
  await opened("Source record");
  expect(trail()).toEqual(["Notes", "Source record"]);
});

test("a kept related list the server refuses, or one this client did not write, is not opened", async () => {
  const related = await relatedList();
  refuseList = true;
  await expect(load()).rejects.toThrow("You may not read these records.");
  await recovery();
  expect(root.querySelector("tbody tr[data-id='9']")).toBeNull();

  refuseList = false;
  const data = JSON.parse(sessionStorage.getItem(NAVIGATION_KEY)!) as { actions: [string, { result: Record<string, unknown> }][] };
  const kept = data.actions.find(([id]) => id === related)!;
  for (const result of [{ ...kept[1].result, kind: "window" }, { ...kept[1].result, domain: "[]" }, { ...kept[1].result, extra: 1 }]) {
    kept[1].result = result;
    sessionStorage.setItem(NAVIGATION_KEY, JSON.stringify(data));
    calls.length = 0;
    await expect(load()).rejects.toThrow("This related action has expired. Open its originating menu again.");
    await recovery();
    expect(calls.filter(call => call.endsWith("erp_normalize_action_result"))).toEqual([]);
    expect(root.querySelector("tbody tr[data-id='9']")).toBeNull();
  }
});

/** Notes list › Source record › the one company its button opened, as a form. */
async function relatedRecord(): Promise<void> {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  await vi.waitFor(() => expect(headerButton("Open company").disabled).toBe(false));
  headerButton("Open company").click();
  await opened("Company record");
}

test("a button that returns one record opens its form on top of the page, and a reload keeps it", async () => {
  await relatedRecord();
  const params = new URLSearchParams(location.search);
  expect([params.get("id"), params.get("view_type"), params.get("model")]).toEqual(["9", "form", "res.company"]);
  expect(params.get("method_action")).not.toBeNull();
  // No list in between: the form opens straight away.
  expect(calls).not.toContain("res.company/web_search_read");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
  admitted.length = 0;
  await load();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
  // The kept result went to the server again as that record, never as a list domain.
  expect(admitted).toEqual([{ type: "ir.actions.act_window", name: "Company", res_model: "res.company", target: "current", views: [[3, "form"]], res_id: 9, context: { create: false } }]);
  back();
  await opened("Source record");
  expect(trail()).toEqual(["Notes", "Source record"]);
});

test("a one-record result the server refuses opens nothing, and a kept one this client did not write is not sent", async () => {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  await vi.waitFor(() => expect(headerButton("Open company").disabled).toBe(false));
  refused.add(9);
  headerButton("Open company").click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("does not exist or you may not read it"));
  expect(new URLSearchParams(location.search).get("id")).toBe("5");
  expect(trail()).toEqual(["Notes", "Source record"]);
  document.querySelector<HTMLButtonElement>("dialog.client-error-dialog [data-error-close]")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")).toBeNull());

  // The method ran but its result was refused, so the record stays locked until the page is read again.
  expect(headerButton("Open company").disabled).toBe(true);

  // Admitted once, refused when a reload sends it again: the form does not open.
  refused.delete(9);
  await load();
  await opened("Source record");
  await vi.waitFor(() => expect(headerButton("Open company").disabled).toBe(false));
  headerButton("Open company").click();
  await opened("Company record");
  const related = new URLSearchParams(location.search).get("method_action")!;
  refused.add(9);
  await expect(load()).rejects.toThrow("does not exist or you may not read it");
  await recovery();
  expect(root.querySelector('input[name="name"]')).toBeNull();

  // A kept one-record result the decoder does not admit never reaches the server.
  refused.delete(9);
  const data = JSON.parse(sessionStorage.getItem(NAVIGATION_KEY)!) as { actions: [string, { result: Record<string, unknown> }][] };
  const kept = data.actions.find(([id]) => id === related)!;
  const original = kept[1].result;
  for (const result of [{ ...original, views: [{ id: 4, type: "list" }, { id: 3, type: "form" }] }, { ...original, domain: [["id", "=", 10]] }, { ...original, res_id: 0 }]) {
    kept[1].result = result;
    sessionStorage.setItem(NAVIGATION_KEY, JSON.stringify(data));
    calls.length = 0;
    await expect(load()).rejects.toThrow("This related action has expired. Open its originating menu again.");
    await recovery();
    expect(calls.filter(call => call.endsWith("erp_normalize_action_result"))).toEqual([]);
  }
});

test("a crumb the server fails to open (not a refusal) stays in the trail; a refused one goes and the focus stays on the trail", async () => {
  await depthThree();
  failing.add(9);
  back();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("The server failed to read the record."));
  expect(trail()).toEqual(["Notes", "Source record", "Company record", "Parent record"]);
  document.querySelector<HTMLButtonElement>("dialog.client-error-dialog [data-error-close]")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")).toBeNull());

  failing.delete(9);
  refused.add(9);
  const crumb = root.querySelector<HTMLButtonElement>(".breadcrumb-previous .breadcrumb-back")!;
  crumb.focus();
  crumb.click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("You may not read this record."));
  expect(trail()).toEqual(["Notes", "Source record", "Parent record"]);
  document.querySelector<HTMLButtonElement>("dialog.client-error-dialog [data-error-close]")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")).toBeNull());
  expect(document.activeElement).toBe(root.querySelector(".breadcrumb-previous .breadcrumb-back"));
});

test("a page load without its history entry numbers past the kept entries, so Back to an older entry gets its own trail", async () => {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  open("Company: Company record");
  await opened("Company record");
  const older = { state: history.state as { erpIndex: number }, url: location.pathname + location.search };
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);

  // A full page load into a new entry (the company switcher), then pages of its own.
  history.pushState(null, "", "/erp/action-21");
  await load();
  await listed(9);
  root.querySelector<HTMLElement>("tbody tr[data-id='9']")!.click();
  await opened("Company record");
  open("Note: Parent record");
  await opened("Parent record");
  expect(trail()).toEqual(["Companies", "Company record", "Parent record"]);
  expect((history.state as { erpIndex: number }).erpIndex).toBeGreaterThan(older.state.erpIndex);

  // Back to the older entry loads its page again with the trail it had.
  history.replaceState(older.state, "", older.url);
  await load();
  await opened("Company record");
  expect(trail()).toEqual(["Notes", "Source record", "Company record"]);
});

test("a kept trail whose last entry is another record of the same action is not shown with this page", async () => {
  await depthThree();
  const data = stored()!;
  const index = (history.state as { erpIndex: number }).erpIndex;
  const entries = data.trails.find(([at]) => at === index)![1];
  // The page is Parent record (id 6); the kept trail ends on Source record (id 5) of the same action.
  sessionStorage.setItem(NAVIGATION_KEY, JSON.stringify({ ...data, trails: [[index, [entries[0], entries[2], entries[1]]]] }));
  await load();
  await opened("Parent record");
  expect(trail()).toEqual(["Notes", "Parent record"]);
});

test("a request that fails after the session expired does not write the navigation store again", async () => {
  history.replaceState(null, "", "/erp/action-20");
  await load();
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await opened("Source record");
  await vi.waitFor(() => expect(headerButton("Note companies").disabled).toBe(false));
  expect(sessionStorage.getItem(NAVIGATION_KEY)).not.toBeNull();
  // The button's list opens, and the read of its views meets the expired session: the open fails, and its catch drops
  // the related list it had kept.
  expireOn = "res.company/get_views";
  headerButton("Note companies").click();
  await vi.waitFor(() => expect(root.textContent).toContain("Session expired"));
  await new Promise(resolve => setTimeout(resolve, 50));
  expect(sessionStorage.getItem(NAVIGATION_KEY)).toBeNull();
});

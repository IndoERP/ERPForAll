/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates, registerListHeaderExtension } from "../src/client/registry.js";
import type { ListHeaderEnvironment, ListHeaderHandle } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";

const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" },
};
const field = { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" };
const description = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note",
  fields: { "test.note": { name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null } } },
  notices: [],
  views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field] },
  ],
};

/** One registration per model for the whole file (identities are unique); each test sets the behaviour. */
type Behaviour = { refresh: (domain: unknown[], signal: AbortSignal) => Promise<void>; secondThrows?: boolean };
let behaviour: Behaviour = { refresh: async () => {} };
const created: ListHeaderEnvironment[] = [], refreshed: unknown[][] = [], signals: AbortSignal[] = [];
let disposed = 0, secondDisposed = 0;
registerListHeaderExtension("test.note_header", {
  model: "test.note",
  create(env): ListHeaderHandle {
    created.push(env);
    const element = document.createElement("section");
    element.className = "test-note-header";
    element.textContent = `${env.title} header`;
    return {
      element,
      refresh: (domain, { signal }) => { refreshed.push(domain); signals.push(signal); return behaviour.refresh(domain, signal); },
      dispose: () => { disposed += 1; },
    };
  },
});
registerListHeaderExtension("test.note_second_header", {
  model: "test.note",
  create(): ListHeaderHandle {
    if (behaviour.secondThrows) throw new Error("second header cannot start");
    return { element: document.createElement("div"), refresh: async () => {}, dispose: () => { secondDisposed += 1; } };
  },
});
registerListHeaderExtension("test.other_header", {
  model: "test.other",
  create: () => { throw new Error("a header for another model must never be created"); },
});

let app: HumanApp | undefined;
const showModalDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const closeDialogDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
beforeEach(() => {
  // jsdom has no modal dialogs: the error dialog needs them, as in client-app.test.ts.
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  created.length = 0; refreshed.length = 0; signals.length = 0; disposed = 0; secondDisposed = 0; behaviour = { refresh: async () => {} };
  configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
      const node = document.createElement("div"); node.setAttribute(`data-control-${key}`, ""); root.append(node);
    }
    return root;
  } }]);
  history.replaceState(null, "", "/erp/action-20?view_type=list");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views"))
        ? (() => { const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }], domain: null, context: null };
          return path.endsWith("erp_load_web_action_views") ? { action, views: description } : action; })()
      : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Notes" }] } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
});
afterEach(() => {
  if (showModalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", showModalDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (closeDialogDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDialogDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
});
afterEach(() => { app?.dispose(); app = undefined; vi.unstubAllGlobals(); vi.restoreAllMocks(); document.body.replaceChildren(); history.replaceState(null, "", "/"); });

function host(): HTMLElement {
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node);
  }
  document.body.append(root);
  return root;
}

test("a list header of the action's model sits above the rows with the list's own data access", async () => {
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  const header = await vi.waitFor(() => { const node = root.querySelector<HTMLElement>(".test-note-header"); expect(node).not.toBeNull(); return node!; });
  expect(header.textContent).toBe("Notes header");
  await vi.waitFor(() => expect(root.textContent).toContain("Meeting Notes"));
  const row = [...root.querySelectorAll<HTMLElement>("*")].find(node => node.children.length === 0 && node.textContent === "Meeting Notes")!;
  expect(header.compareDocumentPosition(row) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(created).toHaveLength(1);
  expect(created[0]).toMatchObject({ model: "test.note", title: "Notes" });
  expect(typeof created[0]!.rpc.call).toBe("function");
  expect(created[0]!.context).toBeTypeOf("object");
  await vi.waitFor(() => expect(refreshed).toEqual([[]]));
});

test("every list reload refreshes the header with the domain the rows were read with", async () => {
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(refreshed).toHaveLength(1));
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.value = "Meeting";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit"));
  await vi.waitFor(() => expect(refreshed).toHaveLength(2));
  expect(JSON.stringify(refreshed[1])).toContain("Meeting");
  expect(created).toHaveLength(1);
});

test("a failing header refresh is reported and the rows still load", async () => {
  behaviour = { refresh: async () => { throw new Error("header counts unavailable"); } };
  const reported = vi.spyOn(HumanApp.prototype as unknown as { reportError(error: unknown): void }, "reportError");
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(root.textContent).toContain("Meeting Notes"));
  await vi.waitFor(() => expect(reported.mock.calls.some(([error]) => error instanceof Error && error.message === "header counts unavailable")).toBe(true));
});

test("leaving the list disposes its header", async () => {
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(created).toHaveLength(1));
  app.dispose(); app = undefined;
  expect(disposed).toBe(1);
});

test("a header that cannot start fails the list loudly and disposes the headers created before it", async () => {
  behaviour = { refresh: async () => {}, secondThrows: true };
  const root = host(); app = new HumanApp(root, bootstrap);
  await expect(app.start()).rejects.toThrow("second header cannot start");
  expect(created).toHaveLength(1);
  expect(disposed).toBe(1);
  expect(root.querySelector(".test-note-header")).toBeNull();
});

test("a refresh that throws synchronously is reported and never stops the rows", async () => {
  behaviour = { refresh: () => { throw new Error("thrown before any promise"); } };
  const reported = vi.spyOn(HumanApp.prototype as unknown as { reportError(error: unknown): void }, "reportError");
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(root.textContent).toContain("Meeting Notes"));
  await vi.waitFor(() => expect(reported.mock.calls.some(([error]) => error instanceof Error && error.message === "thrown before any promise")).toBe(true));
});

test("a newer load aborts the previous refresh, whose late failure is not reported; leaving aborts the last", async () => {
  let failLate: ((error: Error) => void) | undefined;
  behaviour = { refresh: () => refreshed.length === 1 ? new Promise<void>((_, reject) => { failLate = reject; }) : Promise.resolve() };
  const reported = vi.spyOn(HumanApp.prototype as unknown as { reportError(error: unknown): void }, "reportError");
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(signals).toHaveLength(1));
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.value = "Meeting";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit"));
  await vi.waitFor(() => expect(signals).toHaveLength(2));
  expect(signals[0]!.aborted).toBe(true);
  expect(signals[1]!.aborted).toBe(false);
  failLate!(new Error("stale counts"));
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(reported.mock.calls.some(([error]) => error instanceof Error && error.message === "stale counts")).toBe(false);
  app.dispose(); app = undefined;
  expect(signals[1]!.aborted).toBe(true);
});

test("each header gets its own copy of the domain", async () => {
  behaviour = { refresh: async domain => { domain.push(["mutated", "=", true]); } };
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(refreshed).toHaveLength(1));
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  input.value = "Meeting";
  root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit"));
  await vi.waitFor(() => expect(refreshed).toHaveLength(2));
  expect(JSON.stringify(refreshed[1]!.slice(0, -1))).not.toContain("mutated");
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { configureClientTemplates, fieldWidgets, registerFieldWidget, type RecordEvent } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import { ErrorDialog } from "../src/client/error-dialog.js";
import { describeViews } from "../src/client/metadata.js";
import { el } from "../src/client/dom.js";

const modifiers = { readonly: null, required: null, invisible: null };
const textField = (label: string) => ({ type: "char", label, required: false, readonly: false, selection: null, relation: null, relation_field: null });
const fieldNode = (name: string, widget = "char") => ({ kind: "field", force_save: false, field: { model: "test.book", name }, widget, options: {}, modifiers, label: name });
const formNodes = (root: Record<string, unknown> = { edit_mode: "read" }) => [
  { kind: "form", record_mode: "record", children: [1, 2], ...root }, fieldNode("name"), fieldNode("note", "test_record_events"),
];
const descriptionOf = (nodes: unknown[]) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.book",
  fields: { "test.book": { name: textField("Name"), note: textField("Note") } },
  views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes }], notices: [],
});

/** A widget that records what the form tells it: its mode at render and every record event. */
const heard: { mode: string | undefined; events: RecordEvent[]; listening: number } = { mode: undefined, events: [], listening: 0 };
let throwing = false;
if (!fieldWidgets.has("test_record_events")) registerFieldWidget("test_record_events", { kinds: ["char"], render: environment => {
  heard.mode = environment.mode;
  heard.listening++;
  const stop = environment.onRecordEvent!(event => { heard.events.push(event); if (throwing) throw new Error("listener failed"); });
  return { node: el("span", String(environment.value ?? ""), "field-read-value"), dispose: () => { heard.listening--; stop(); } };
} });

describe("form edit_mode metadata", () => {
  test("read and edit are admitted on a form; anything else, or read on a read-only form, is refused", () => {
    expect(describeViews(descriptionOf(formNodes()), "test.book", "r1").views.get("form")!.editMode).toBe("read");
    expect(describeViews(descriptionOf(formNodes({ edit_mode: "edit" })), "test.book", "r1").views.get("form")!.editMode).toBe("edit");
    expect(describeViews(descriptionOf(formNodes({})), "test.book", "r1").views.get("form")!.editMode).toBeUndefined();
    expect(() => describeViews(descriptionOf(formNodes({ edit_mode: "view" })), "test.book", "r1")).toThrow("Invalid form edit mode.");
    expect(() => describeViews(descriptionOf(formNodes({ edit_mode: "read", edit: false })), "test.book", "r1")).toThrow("Read mode requires an editable record form.");
  });
});

describe("read-first record form", () => {
  const bootstrap = {
    wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true },
    companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
    locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "test-token-not-secret" },
  };
  let app: HumanApp | undefined;
  const presented: unknown[] = [];
  beforeEach(() => {
    presented.length = 0; heard.mode = undefined; heard.events = []; heard.listening = 0; throwing = false;
    vi.spyOn(ErrorDialog.prototype, "show").mockImplementation(error => { presented.push(error); });
    Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
    Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
    configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
      const root = document.createElement("section");
      root.setAttribute("data-control-panel", "");
      for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const part = document.createElement("div"); part.setAttribute(`data-control-${key}`, ""); root.append(part); }
      return root;
    } }]);
  });
  afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); history.replaceState(null, "", "/"); });

  const start = async (options: { nodes?: unknown[]; fresh?: boolean; refuseSave?: boolean } = {}) => {
    const nodes = options.nodes ?? formNodes();
    history.replaceState(null, "", options.fresh ? "/erp/action-20?view_type=form" : "/erp/action-20?id=5&view_type=form");
    const calls: string[] = [];
    let stored = "Dune";
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as Record<string, unknown>;
      calls.push(path.split("/").pop()!);
      if (path.endsWith("web_save") && options.refuseSave) return new Response(JSON.stringify({ name: "erp.exceptions.UserError", message: "The book cannot be saved." }), { status: 422, headers: { "Content-Type": "application/json" } });
      if (path.endsWith("web_save")) stored = String((body["vals"] as Record<string, unknown>)["name"] ?? stored);
      const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Books", action_id: 20, children: [], icon: null }] }
        : path.endsWith("erp_load_web_action_views") ? { action: { wire_major: 1, id: 20, name: "Books", model: "test.book", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, views: descriptionOf(nodes) }
        : path.endsWith("erp_load_web_action") ? { wire_major: 1, id: 20, name: "Books", model: "test.book", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }
        : path.endsWith("get_views") ? descriptionOf(nodes)
        : path.endsWith("onchange") ? { value: {} }
        : path.endsWith("default_get") ? {}
        : [{ id: 5, name: stored, note: "kept" }];
      return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
    }));
    const root = document.createElement("div");
    for (const region of ["header", "navigation", "notice", "content"]) { const part = document.createElement("div"); part.setAttribute(`data-client-${region}`, ""); root.append(part); }
    document.body.append(root);
    app = new HumanApp(root, bootstrap);
    await app.start();
    await vi.waitFor(() => expect(root.querySelector("form.record-form [data-field]")).not.toBeNull());
    return { root, calls };
  };
  const nameInput = (root: HTMLElement) => root.querySelector<HTMLInputElement>('form.record-form input[name="name"]');
  const editButton = (root: HTMLElement) => root.querySelector<HTMLButtonElement>(".status-btn-edit")!;
  const typeName = async (root: HTMLElement, value: string) => {
    const input = nameInput(root)!; input.value = value; input.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(nameInput(root)?.disabled).toBe(false));
  };

  test("a saved record opens read-first: no field inputs, an Edit button, no Save or Discard", async () => {
    const { root } = await start();
    expect(nameInput(root)).toBeNull();
    expect(root.querySelector("form.record-form")!.classList.contains("record-form-read")).toBe(true);
    expect(root.querySelector("form.record-form")!.querySelectorAll("input,textarea,select")).toHaveLength(0);
    expect(editButton(root).hidden).toBe(false);
    expect(root.querySelector<HTMLElement>(".status-actions")!.style.display).toBe("none");
    expect(heard.mode).toBe("read");
  });

  test("Edit switches to the editable form and tells the widgets", async () => {
    const { root } = await start();
    editButton(root).click();
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
    expect(editButton(root).hidden).toBe(true);
    expect(root.querySelector("form.record-form")!.classList.contains("record-form-read")).toBe(false);
    expect(heard.mode).toBe("edit");
    expect(heard.events).toEqual([{ type: "modeChanged", mode: "edit" }]);
  });

  test("the E key edits when the focus is not in a field", async () => {
    const { root } = await start();
    document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "e", bubbles: true }));
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
  });

  test("a successful save returns to read mode; widgets hear saved with the id, then the mode", async () => {
    const { root, calls } = await start();
    editButton(root).click();
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
    await typeName(root, "Dune Messiah");
    heard.events = [];
    root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
    await vi.waitFor(() => expect(nameInput(root)).toBeNull());
    expect(calls).toContain("web_save");
    expect(root.querySelector("form.record-form")!.textContent).toContain("Dune Messiah");
    expect(heard.events).toEqual([{ type: "saved", id: 5 }, { type: "modeChanged", mode: "read" }]);
  });

  test("a refused save stays in edit mode and widgets hear saveFailed", async () => {
    const { root } = await start({ refuseSave: true });
    editButton(root).click();
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
    await typeName(root, "Dune Messiah");
    heard.events = [];
    root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
    await vi.waitFor(() => expect(heard.events).toEqual([{ type: "saveFailed" }]));
    expect(nameInput(root)).not.toBeNull();
    expect(editButton(root).hidden).toBe(true);
  });

  test("Discard returns to read mode with the saved values", async () => {
    const { root } = await start();
    editButton(root).click();
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
    await typeName(root, "Something else");
    heard.events = [];
    root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
    await vi.waitFor(() => expect(nameInput(root)).toBeNull());
    expect(root.querySelector("form.record-form")!.textContent).toContain("Dune");
    expect(root.querySelector("form.record-form")!.textContent).not.toContain("Something else");
    expect(heard.events).toEqual([{ type: "discarded" }, { type: "modeChanged", mode: "read" }]);
  });

  test("a new record opens editable", async () => {
    const { root } = await start({ fresh: true });
    expect(nameInput(root)).not.toBeNull();
    expect(editButton(root).hidden).toBe(true);
  });

  test("a form without edit_mode keeps opening editable and still reports saves", async () => {
    const { root } = await start({ nodes: formNodes({}) });
    expect(nameInput(root)).not.toBeNull();
    expect(editButton(root).hidden).toBe(true);
    await typeName(root, "Dune Messiah");
    heard.events = [];
    root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
    await vi.waitFor(() => expect(heard.events).toEqual([{ type: "saved", id: 5 }]));
    expect(nameInput(root)).not.toBeNull();
  });

  test("a throwing listener is reported and the save still completes", async () => {
    const { root, calls } = await start({ nodes: formNodes({}) });
    await typeName(root, "Dune Messiah");
    throwing = true;
    root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
    await vi.waitFor(() => expect(presented.some(error => error instanceof Error && error.message === "listener failed")).toBe(true));
    expect(calls).toContain("web_save");
    expect(root.querySelector<HTMLElement>(".status-actions")!.style.display).toBe("none");
  });

  test("a widget's listener ends with the widget: re-renders never leave old listeners behind", async () => {
    const { root } = await start();
    editButton(root).click();
    await vi.waitFor(() => expect(nameInput(root)).not.toBeNull());
    await typeName(root, "Dune Messiah");
    expect(heard.listening).toBe(1);
    heard.events = [];
    root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
    await vi.waitFor(() => expect(nameInput(root)).toBeNull());
    // one rendered widget, so one copy of each event
    expect(heard.events).toEqual([{ type: "discarded" }, { type: "modeChanged", mode: "read" }]);
    app!.dispose(); app = undefined;
    expect(heard.listening).toBe(0);
  });
});

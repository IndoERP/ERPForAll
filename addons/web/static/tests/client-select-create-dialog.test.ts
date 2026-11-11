/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { selectCreateDialog } from "../src/client/select-create-dialog.js";
import { editRelation } from "../src/client/relation-editor.js";
import { RpcClient } from "../src/client/transport.js";

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); });

/// (partial: callback-driven selection/create; installed record editor and phone layout acceptance are separate)
test("selection applies domain/context, renders readonly records, serializes selection and returns saved create/edit", async () => {
  const args: Record<string, unknown>[] = [];
  const rpc = new RpcClient(async (_url, init) => { args.push(JSON.parse(String(init?.body))); return new Response(JSON.stringify([{ id: 4, display_name: "Ada" }])); }, () => {});
  let finish!: () => void;
  const selected = vi.fn(() => new Promise<void>(resolve => { finish = resolve; }));
  const handle = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [["active", "=", true]], context: { lang: "en_US" }, select: selected });
  await vi.waitFor(() => expect(handle.dialog.querySelector("[data-record-id='4']")).not.toBeNull());
  expect(args[0]).toMatchObject({ domain: [["active", "=", true]], context: { lang: "en_US" } });
  expect(handle.dialog.querySelector("[contenteditable],input:not([type='search'])")).toBeNull();
  const choice = handle.dialog.querySelector<HTMLButtonElement>("[data-record-id='4']")!;
  choice.click(); choice.click(); expect(selected).toHaveBeenCalledTimes(1);
  expect(selected).toHaveBeenCalledWith({ id: 4, display_name: "Ada" });
  finish(); await vi.waitFor(() => expect(handle.dialog.isConnected).toBe(false));
  const create = vi.fn(async () => ({ id: 9, display_name: "New partner" })); const created = vi.fn();
  const next = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [], context: {}, select: created, createEdit: create });
  [...next.dialog.querySelectorAll<HTMLButtonElement>("button")].find(control => control.textContent === "Create and edit")!.click();
  await vi.waitFor(() => expect(created).toHaveBeenCalledWith({ id: 9, display_name: "New partner" }));
  expect(create).toHaveBeenCalledWith({ domain: [], context: {} });
});

test("search responses cannot repopulate a disposed picker", async () => {
  let answer!: (response: Response) => void;
  const rpc = new RpcClient(() => new Promise(resolve => { answer = resolve; }), () => {});
  const handle = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [], context: {}, select: () => {} });
  handle.dispose(); answer(new Response(JSON.stringify([{ id: 1, display_name: "Late" }])));
  await Promise.resolve(); await Promise.resolve(); expect(document.querySelector("dialog")).toBeNull();
});

test.each(["abort", "dispose"])("deferred create never starts after synchronous owner %s", async mode => {
  const scope = new AbortController();
  const rpc = new RpcClient(async () => new Response("[]"), () => {});
  const create = vi.fn(async () => {
    const late = document.createElement("div"); late.dataset["lateCreate"] = ""; document.body.append(late);
    return { id: 9, display_name: "Must not start" };
  });
  const selected = vi.fn();
  const handle = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [], context: {}, select: selected, createEdit: create, signal: scope.signal });
  [...handle.dialog.querySelectorAll<HTMLButtonElement>("button")].find(control => control.textContent === "Create and edit")!.click();
  if (mode === "abort") scope.abort(); else dialogs.dispose();
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  expect(create).not.toHaveBeenCalled(); expect(selected).not.toHaveBeenCalled();
  expect(document.querySelector("[data-late-create],dialog")).toBeNull();
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "Selection dialog options must be a plain object."],
  [{ context: [] }, "Selection context must be a plain object."],
  [{ context: null }, "Selection context must be a plain object."],
  [{ model: "bad/model" }, "Selection model must be a valid name of at most 256 bytes."],
  [{ createEdit: true }, "Selection createEdit must be a function."],
  [{ report: true }, "Selection report must be a function."],
  [{ signal: {} }, "Dialog signal must be an AbortSignal."],
  [{ unknown: true }, "Unknown selection dialog option: unknown."],
])("picker validates context, callbacks and closed options: %j", (patch, message) => {
  const rpc = new RpcClient(async () => new Response("[]"), () => {});
  const options = patch === null ? null : { dialogs, rpc, model: "res.partner", domain: [], context: {}, select: () => {}, ...patch };
  expect(() => selectCreateDialog(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
  expect(document.querySelector("dialog")).toBeNull();
});

test("select-create composes desktop and phone selection with real record save and readback", async () => {
  // Client/protocol fixture acceptance: installed backend authorization and persistence are separate evidence.
  let phone = false, writes = 0, savedName = "";
  vi.stubGlobal("matchMedia", () => ({ matches: phone })); vi.stubGlobal("innerWidth", 1024);
  const domain = [["active", "=", true]], context = { lang: "en_US", default_name: "Suggested partner" };
  const calls: { method: string; args: Record<string, unknown> }[] = [];
  const model = "res.partner";
  const metadata = { wire_major: 1, registry_version: "r1", description_version: "d1", model,
    fields: { [model]: { name: { type: "char", label: "Name", required: true, readonly: false, selection: null, relation: null, relation_field: null } } }, notices: [],
    views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
      nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "field", force_save: false, field: { model, name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }] }] };
  const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  let finishSave!: () => void;
  const rpc = new RpcClient(async (url, init) => {
    const method = String(url).split("/").at(-1)!, args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ method, args });
    if (method === "web_name_search") return json([{ id: 4, display_name: "Existing partner" }]);
    if (method === "get_views") return json(metadata);
    if (method === "default_get") return json({ name: context.default_name });
    if (method === "onchange") return json({ value: {} });
    if (method === "web_save") {
      writes++; const entered = String((args["vals"] as Record<string, unknown>)["name"]);
      return new Promise<Response>(resolve => { finishSave = () => { savedName = entered; resolve(json([{ id: 9, name: entered }])); }; });
    }
    if (method === "web_read") return json([{ id: 9, display_name: `${savedName} (saved)` }]);
    throw new Error(`Unhandled fixture method ${method}`);
  }, () => {});
  let finishSelection!: () => void;
  const selected = vi.fn(() => new Promise<void>(resolve => { finishSelection = resolve; }));
  const desktop = selectCreateDialog({ dialogs, rpc, model, domain, context, select: selected });
  await vi.waitFor(() => expect(desktop.dialog.querySelector(".select-create-list [data-record-id='4']")).not.toBeNull());
  expect(calls[0]).toMatchObject({ method: "web_name_search", args: { domain, context } });
  expect(desktop.dialog.querySelector(".select-create-cards,[contenteditable],input:not([type='search'])")).toBeNull();
  const desktopChoice = desktop.dialog.querySelector<HTMLButtonElement>("[data-record-id='4']")!;
  desktopChoice.click(); desktopChoice.click();
  expect(selected).toHaveBeenCalledTimes(1); expect(selected).toHaveBeenCalledWith({ id: 4, display_name: "Existing partner" });
  expect(desktop.dialog.isConnected).toBe(true); expect(desktopChoice.disabled).toBe(true);
  finishSelection(); await vi.waitFor(() => expect(desktop.dialog.isConnected).toBe(false));

  phone = true; vi.stubGlobal("innerWidth", 390);
  const created = vi.fn(), report = vi.fn();
  const createRequests: { domain: readonly unknown[]; context: Record<string, unknown> }[] = [];
  const mobile = selectCreateDialog({ dialogs, rpc, model, domain, context, select: created, report,
    createEdit: input => { createRequests.push(input); return editRelation({ dialogs, rpc, model, context: input.context, report }); } });
  await vi.waitFor(() => expect(mobile.dialog.querySelector(".select-create-card [data-record-id='4']")).not.toBeNull());
  expect(mobile.dialog.querySelector("table")).toBeNull();
  const searches = calls.filter(call => call.method === "web_name_search");
  expect(searches[1]?.args).toMatchObject({ domain, context });
  const create = [...mobile.dialog.querySelectorAll<HTMLButtonElement>("button")].find(control => control.textContent === "Create and edit")!;
  create.click(); create.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLInputElement>(".relation-editor-body input[name='name']")?.value).toBe(context.default_name));
  expect(createRequests).toEqual([{ domain, context }]);
  expect(calls.filter(call => call.method === "default_get")).toHaveLength(1);
  expect(calls.filter(call => call.method === "onchange")).toHaveLength(1);
  const editor = document.querySelector<HTMLElement>(".relation-editor-body")!.closest("dialog")!;
  const input = editor.querySelector<HTMLInputElement>("input[name='name']")!;
  input.value = "Created through picker"; input.dispatchEvent(new Event("change", { bubbles: true }));
  const save = [...editor.querySelectorAll<HTMLButtonElement>("button")].find(control => control.textContent === "Save")!;
  await vi.waitFor(() => expect(save.disabled).toBe(false)); save.click(); save.click();
  expect(writes).toBe(1); expect(created).not.toHaveBeenCalled(); expect(save.disabled).toBe(true);
  expect(calls.some(call => call.method === "web_read")).toBe(false);
  expect(calls.find(call => call.method === "web_save")?.args).toMatchObject({ ids: [], vals: { name: "Created through picker" }, context });
  finishSave();
  await vi.waitFor(() => expect(created).toHaveBeenCalledWith({ id: 9, display_name: "Created through picker (saved)" }));
  expect(created).toHaveBeenCalledTimes(1); expect(writes).toBe(1);
  expect(calls.at(-1)).toMatchObject({ method: "web_read", args: { ids: [9], specification: { display_name: {} }, context } });
  await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull()); expect(report).not.toHaveBeenCalled();
});

test("the picker opens on its single search box, counts the records and moves through them with the arrow keys", async () => {
  const rpc = new RpcClient(async () => new Response(JSON.stringify([{ id: 4, display_name: "Ada" }, { id: 5, display_name: "Grace" }])), () => {});
  const handle = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [], context: {}, select: () => {} });
  const search = handle.dialog.querySelector<HTMLInputElement>("input[type='search']")!;
  // autofocus: the dialog service hands it to the focus trap as the first focus, not the close button.
  expect(search.autofocus).toBe(true);
  expect(document.activeElement).toBe(search);
  expect(search.getAttribute("aria-label")).toBe("Find a record");
  expect(handle.dialog.querySelector(".select-create-body .field-label")).toBeNull();
  await vi.waitFor(() => expect(handle.dialog.querySelector(".select-create-status")!.textContent).toBe("2 records"));
  expect(handle.dialog.querySelector(".select-create-list th")!.textContent).toBe("Name");
  const key = (target: Element, name: string): void => { target.dispatchEvent(new KeyboardEvent("keydown", { key: name, bubbles: true, cancelable: true })); };
  const ada = handle.dialog.querySelector<HTMLButtonElement>("[data-record-id='4']")!, grace = handle.dialog.querySelector<HTMLButtonElement>("[data-record-id='5']")!;
  search.focus(); key(search, "ArrowDown"); expect(document.activeElement).toBe(ada);
  key(ada, "ArrowDown"); expect(document.activeElement).toBe(grace);
  key(grace, "ArrowDown"); expect(document.activeElement).toBe(grace);
  key(grace, "ArrowUp"); expect(document.activeElement).toBe(ada);
  key(ada, "ArrowUp"); expect(document.activeElement).toBe(search);
});

test("a full page of 80 names says the list is cut; an empty search clears the count", async () => {
  let rows = Array.from({ length: 80 }, (_, i) => ({ id: i + 1, display_name: `Partner ${i + 1}` }));
  const rpc = new RpcClient(async () => new Response(JSON.stringify(rows)), () => {});
  const handle = selectCreateDialog({ dialogs, rpc, model: "res.partner", domain: [], context: {}, select: () => {} });
  const status = handle.dialog.querySelector(".select-create-status")!;
  await vi.waitFor(() => expect(status.textContent).toBe("First 80 records. Type to narrow the search."));
  rows = [];
  const search = handle.dialog.querySelector<HTMLInputElement>("input[type='search']")!;
  search.value = "nobody"; search.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(handle.dialog.querySelector(".select-create-results")!.textContent).toBe("No matching records."));
  expect(status.textContent).toBe("");
});

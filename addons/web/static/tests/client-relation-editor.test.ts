/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { editRelation } from "../src/client/relation-editor.js";
import { RpcClient } from "../src/client/transport.js";

const model = "test.partner";
const field = { type: "char", label: "Name", required: true, readonly: false, selection: null, relation: null, relation_field: null };
const description = () => ({ wire_major: 1, registry_version: "r1", description_version: "d1", model,
  fields: { [model]: { name: field } }, notices: [], views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
    nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "field", force_save: false, field: { model, name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }] }] });
const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
let dialogs: DialogService;
const errors: unknown[] = [];
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(error => errors.push(error)); errors.length = 0;
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });
const control = (label: string): HTMLButtonElement => [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(item => item.textContent === label)!;

/// (partial: real client RecordForm/default_get/onchange/web_save/readback adapter; installed authorization and phone geometry separate)
test("new relation uses the real record form and returns the saved readback selection without navigation", async () => {
  const calls: { method: string; args: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (url, init) => {
    const method = String(url).split("/").at(-1)!; const args = JSON.parse(String(init?.body)); calls.push({ method, args });
    return json(method === "get_views" ? description() : method === "default_get" ? { name: "Suggested" } : method === "onchange" ? { value: {} } : method === "web_save" ? [{ id: 17, name: "Typed" }] : [{ id: 17, display_name: "Typed partner" }]);
  }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: { default_name: "Suggested" }, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector<HTMLInputElement>("dialog input[name='name']")?.value).toBe("Suggested"));
  const input = document.querySelector<HTMLInputElement>("dialog input[name='name']")!; input.value = "Typed"; input.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(control("Save").disabled).toBe(false)); control("Save").click();
  await expect(result).resolves.toEqual({ id: 17, display_name: "Typed partner" });
  expect(calls.find(call => call.method === "web_save")?.args).toMatchObject({ ids: [], vals: { name: "Typed" }, context: { default_name: "Suggested" } });
  expect(calls.at(-1)).toMatchObject({ method: "web_read", args: { ids: [17], specification: { display_name: {} } } });
  expect(document.querySelector("dialog")).toBeNull(); expect(errors).toEqual([]);
});

test("invalid required field does not save, cancel returns no selection", async () => {
  const writes = vi.fn();
  const rpc = new RpcClient(async url => { const method = String(url).split("/").at(-1); if (method === "web_save") writes(); return json(method === "get_views" ? description() : method === "default_get" ? { name: "" } : { value: {} }); }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector("dialog input[name='name']")).not.toBeNull());
  const input = document.querySelector<HTMLInputElement>("dialog input[name='name']")!;
  input.focus();
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "s", altKey: true, bubbles: true, cancelable: true }));
  expect(writes).not.toHaveBeenCalled(); expect(document.querySelector("[role='alert']")?.textContent).toContain("needs a value");
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "x", altKey: true, bubbles: true, cancelable: true }));
  expect(document.querySelector("dialog")).toBeNull();
  await expect(result).resolves.toBeUndefined();
});

test("existing relation reads then saves its identity; uncertain save cannot be repeated or dismissed", async () => {
  let writes = 0;
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1);
    if (method === "web_save") { writes++; throw new Error("Lost answer"); }
    return json(method === "get_views" ? description() : [{ id: 5, name: "Existing" }]);
  }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, id: 5, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector<HTMLInputElement>("dialog input")?.value).toBe("Existing"));
  control("Save").click(); await vi.waitFor(() => expect(errors).toHaveLength(1));
  expect(errors[0]).toMatchObject({ kind: "ambiguous", message: "The connection was interrupted. The change may have been saved. Check the record before trying again." });
  expect(control("Save").disabled).toBe(true); control("Save").click(); control("Cancel").click();
  expect(writes).toBe(1); expect(document.querySelector("dialog")).not.toBeNull();
  dialogs.dispose(); await expect(result).resolves.toBeUndefined();
});

test("Save shortcut stops applying once only the saved record's readback remains", async () => {
  const methods: string[] = [];
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1)!; methods.push(method);
    if (method === "web_read") return json([]);
    return json(method === "get_views" ? description() : method === "default_get" ? { name: "Partner" }
      : method === "onchange" ? { value: {} } : [{ id: 17, name: "Partner" }]);
  }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error) });
  await vi.waitFor(() => expect(control("Save").disabled).toBe(false));
  control("Save").click();
  await vi.waitFor(() => expect(control("Read saved record")).toBeDefined());
  const retry = control("Read saved record");
  expect(retry.hasAttribute("data-hotkey")).toBe(false);
  const key = new KeyboardEvent("keydown", { key: "s", altKey: true, bubbles: true, cancelable: true });
  retry.dispatchEvent(key);
  expect(key.defaultPrevented).toBe(false);
  expect(methods.filter(method => method === "web_save" || method === "web_read")).toEqual(["web_save", "web_read"]);
  control("Cancel").click(); await expect(result).resolves.toBeUndefined();
});

test("scope abortion cancels metadata reads and late completion cannot mount a record form", async () => {
  let readSignal: AbortSignal | null | undefined;
  const rpc = new RpcClient(async (_url, init) => { readSignal = init?.signal; return new Promise<Response>((_resolve, reject) => init?.signal?.addEventListener("abort", () => reject(new DOMException("Aborted", "AbortError")), { once: true })); }, () => {});
  const scope = new AbortController(); const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error), signal: scope.signal });
  scope.abort(); await expect(result).resolves.toBeUndefined(); expect(readSignal?.aborted).toBe(true);
  expect(document.querySelector("dialog")).toBeNull(); expect(errors).toEqual([]);
});

test("permission refusal retains the entered record, reports the precise error and never retries automatically", async () => {
  let writes = 0;
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1);
    if (method === "web_save") { writes++; return new Response(JSON.stringify({ message: "Partner creation refused." }), { status: 403 }); }
    return json(method === "get_views" ? description() : method === "default_get" ? { name: "Entered" } : { value: {} });
  }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector<HTMLInputElement>("dialog input")?.value).toBe("Entered"));
  control("Save").click(); await vi.waitFor(() => expect(errors).toHaveLength(1));
  expect(errors[0]).toMatchObject({ kind: "permission", message: "Partner creation refused." });
  expect(document.querySelector<HTMLInputElement>("dialog input")?.value).toBe("Entered"); expect(writes).toBe(1);
  control("Cancel").click(); await expect(result).resolves.toBeUndefined();
});

test("Cancel during metadata loading aborts the read and resolves without a selection", async () => {
  let readSignal: AbortSignal | null | undefined;
  const rpc = new RpcClient(async (_url, init) => { readSignal = init?.signal; return new Promise<Response>((_resolve, reject) => init?.signal?.addEventListener("abort", () => reject(new DOMException("Aborted", "AbortError")), { once: true })); }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error) });
  control("Cancel").click(); await expect(result).resolves.toBeUndefined();
  expect(readSignal?.aborted).toBe(true); expect(errors).toEqual([]);
});

/// (partial: relation owner locks ordinary controls across code commit and write)
test("relation Save keeps ordinary fields disabled after a slow code onchange", async () => {
  const d = description();
  Object.assign(d.fields[model], { source: { ...field, type: "text", required: false } });
  d.views[0]!.nodes[0]!.children!.push(2);
  d.views[0]!.nodes.push({ kind: "field", force_save: false, field: { model, name: "source" }, widget: "code", options: { mode: "xml" }, modifiers: { readonly: null, required: null, invisible: null }, label: "Source" });
  let releaseChange!: () => void, releaseSave!: () => void, writes = 0, changes = 0;
  const changing = new Promise<void>(resolve => { releaseChange = resolve; });
  const saving = new Promise<void>(resolve => { releaseSave = resolve; });
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1);
    if (method === "onchange" && ++changes > 1) await changing;
    if (method === "web_save") { writes++; await saving; }
    return json(method === "get_views" ? d : method === "default_get" ? { name: "Partner", source: "<form/>" } : method === "onchange" ? { value: {} } : method === "web_save" ? [{ id: 17, name: "Partner", source: "draft" }] : [{ id: 17, display_name: "Partner" }]);
  }, () => {});
  const result = editRelation({ dialogs, rpc, model, context: {}, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector(".code-field .ace_editor")).not.toBeNull());
  const { default: ace } = await import("ace-builds"); ace.edit(document.querySelector<HTMLElement>(".ace_editor")!).setValue("draft", -1); await Promise.resolve();
  control("Save").click(); await vi.waitFor(() => expect(changes).toBe(2));
  releaseChange(); await vi.waitFor(() => expect(writes).toBe(1));
  expect(document.querySelector<HTMLInputElement>("dialog input[name='name']")?.disabled).toBe(true);
  releaseSave(); await expect(result).resolves.toEqual({ id: 17, display_name: "Partner" }); expect(errors).toEqual([]);
});

/** The form with a read-only link to another partner, the case a record opened from the editor starts from. */
const linked = () => {
  const d = description();
  Object.assign(d.fields[model], { parent_id: { ...field, type: "many2one", label: "Parent", required: false, readonly: true, relation: model } });
  d.views[0]!.nodes[0]!.children!.push(2);
  d.views[0]!.nodes.push({ kind: "field", force_save: false, field: { model, name: "parent_id" }, widget: "many2one", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Parent" });
  return d;
};

test("the page's view loader describes the editor's form, so the editor sends no get_views of its own", async () => {
  const methods: string[] = [];
  const rpc = new RpcClient(async url => { const method = String(url).split("/").at(-1)!; methods.push(method); return json([{ id: 5, name: "Existing" }]); }, () => {});
  const asked: unknown[] = [];
  const views = async (target: string, requested: [number | null, string][], scope: Record<string, unknown>): Promise<unknown> => { asked.push([target, requested, scope]); return description(); };
  const result = editRelation({ dialogs, rpc, model, context: { lang: "en_US" }, id: 5, views, report: error => errors.push(error) });
  await vi.waitFor(() => expect(document.querySelector<HTMLInputElement>("dialog input")?.value).toBe("Existing"));
  expect(asked).toEqual([[model, [[null, "form"]], { lang: "en_US" }]]);
  expect(methods).not.toContain("get_views");
  control("Cancel").click(); await expect(result).resolves.toBeUndefined(); expect(errors).toEqual([]);
});

test("a related record opened from the editor closes the editor first, and is refused while it holds an edit", async () => {
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1);
    return json(method === "get_views" ? linked() : method === "onchange" ? { value: {} } : [{ id: 5, name: "Existing", parent_id: { id: 6, display_name: "Parent partner" } }]);
  }, () => {});
  const opened: [string, number, boolean][] = [];
  const openRecord = async (target: string, id: number): Promise<void> => { opened.push([target, id, document.querySelector("dialog") !== null]); };
  const open = (): HTMLButtonElement => document.querySelector<HTMLButtonElement>('dialog [aria-label="Open Parent: Parent partner"]')!;
  // An edit in progress is not dropped: the editor stays open and says why.
  const first = editRelation({ dialogs, rpc, model, context: {}, id: 5, openRecord, report: error => errors.push(error) });
  await vi.waitFor(() => expect(open()).not.toBeNull());
  const input = document.querySelector<HTMLInputElement>("dialog input[name='name']")!; input.value = "Edited"; input.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(control("Save").disabled).toBe(false));
  open().click();
  await vi.waitFor(() => expect(errors).toEqual([expect.objectContaining({ kind: "validation", message: "Save or cancel this record before opening another one." })]));
  expect(opened).toEqual([]); expect(document.querySelector("dialog")).not.toBeNull();
  control("Cancel").click(); await expect(first).resolves.toBeUndefined(); errors.length = 0;
  // Without an edit the editor closes, then the record opens on the page.
  const second = editRelation({ dialogs, rpc, model, context: {}, id: 5, openRecord, report: error => errors.push(error) });
  await vi.waitFor(() => expect(open()).not.toBeNull());
  open().click();
  await expect(second).resolves.toBeUndefined();
  await vi.waitFor(() => expect(opened).toEqual([[model, 6, false]]));
  expect(errors).toEqual([]);
});

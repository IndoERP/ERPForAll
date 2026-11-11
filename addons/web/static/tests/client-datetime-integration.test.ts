/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { createFieldControl, fieldControl } from "../src/client/fields.js";
import { HumanApp } from "../src/client/app.js";
import { MessageServices } from "../src/client/messages.js";
import type { Field, View } from "../src/client/metadata.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { fieldWidgets, registerFieldWidget, type WidgetEnvironment } from "../src/client/registry.js";
import { editRelation } from "../src/client/relation-editor.js";
import { RpcClient } from "../src/client/transport.js";
import { WizardDialog } from "../src/client/wizard-dialog.js";

const locale = { lang: "en_GB", tz: "Asia/Jakarta", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" };
const context = { lang: "en_GB", tz: "Asia/Jakarta", allowed_company_ids: [1] };
const field = (name: string, kind: Field["kind"]): Field => ({ name, kind, label: name, required: false, readonly: false, relation: null, relationField: null, selection: null });
const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const probeEnvironments: (WidgetEnvironment & { editorLocale?: typeof locale })[] = [];
registerFieldWidget("datetime_integration_probe", { kinds: ["char"], render: environment => {
  probeEnvironments.push(environment); const input = document.createElement("input"); input.name = environment.field.name;
  return { node: input, dispose: () => {} };
} });
const view = (fields: Field[], widget?: string): View => ({ type: "form", mode: "record", fields, nodes: [{ kind: "root", children: fields.map(candidate => ({ kind: "field", field: candidate, label: candidate.label, widget: widget ?? candidate.kind })) }] });
const stops: (() => void)[] = [];
let messages: MessageServices;
const errors: unknown[] = [];
const report = (error: unknown): void => { errors.push(error); };
const modalDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const closeDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  messages = new MessageServices(document.body, report); probeEnvironments.length = 0; errors.length = 0;
});
afterEach(() => {
  for (const stop of stops.splice(0).reverse()) stop(); messages.dispose(); document.body.replaceChildren(); vi.restoreAllMocks();
  if (modalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", modalDescriptor); else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (closeDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDescriptor); else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
});
const environment = (target: Field, value: unknown, change = vi.fn(), signal?: AbortSignal) => ({ field: target, value, change, rpc: new RpcClient(fetch, () => {}), context, messages, editorLocale: locale, signal, report, run: (job: () => Promise<void>): void => { void job().catch(report); } });

/// (partial: builtin field adapter preserves ISO dates; reusable picker behavior is tested separately)
test("builtin date uses configured display format and retains the existing ISO change contract", () => {
  const change = vi.fn(), handle = fieldWidgets.get("date").render(environment(field("scheduled_date", "date"), "2026-11-01", change));
  stops.push(handle.dispose); document.body.append(handle.node);
  const input = handle.node.querySelector<HTMLInputElement>("input")!;
  expect(input.type).toBe("text"); expect(input.value).toBe("01/11/2026");
  input.value = "02/11/2026"; input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).toHaveBeenCalledExactlyOnceWith("2026-11-02");
});

/// (partial: builtin timezone display and UTC change contract)
test("builtin datetime converts the configured zone to UTC without adding display settings to RPC context", () => {
  const change = vi.fn(), handle = fieldWidgets.get("datetime").render(environment(field("appointment", "datetime"), "2026-11-01 09:00:00", change));
  stops.push(handle.dispose); document.body.append(handle.node);
  const input = handle.node.querySelector<HTMLInputElement>("input")!;
  expect(input.value).toBe("01/11/2026 16:00:00");
  input.value = "02/11/2026 10:15:00"; input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).toHaveBeenCalledExactlyOnceWith("2026-11-02 03:15:00");
  expect(context).toEqual({ lang: "en_GB", tz: "Asia/Jakarta", allowed_company_ids: [1] });
});

test("date fields refuse an absent editor environment instead of falling back to native input", () => {
  expect(() => fieldControl({ field: field("scheduled_date", "date"), value: false, rpc: new RpcClient(fetch, () => {}), context, change: vi.fn(), run: () => {} })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Date/time fields require an editor locale and the shared popover service." }));
});

test.each(["date", "datetime"] as const)("node-only %s helper refuses missing lifetime ownership before attaching global listeners", kind => {
  const added = vi.spyOn(window, "addEventListener");
  // Release the old implementation's leaked listener even while this regression is RED.
  stops.push(() => { for (const [type, listener, options] of added.mock.calls) if (type === "resize") window.removeEventListener(type, listener, options); });
  const editor = { locale, popovers: messages.popovers, report };
  expect(() => fieldControl({ field: field("scheduled", kind), value: false, rpc: new RpcClient(fetch, () => {}), context, change: vi.fn(), run: () => {}, editor })).toThrowError(expect.objectContaining({
    kind: "validation", message: "The node-only date/time field helper requires an owner AbortSignal; use createFieldControl for explicit disposal.",
  }));
  expect(added.mock.calls.some(([type]) => type === "resize")).toBe(false);
});

test("node-only date helper releases its global listener and overlay when its owner aborts", () => {
  const scope = new AbortController(), change = vi.fn(), added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener");
  const node = fieldControl({ field: field("scheduled", "date"), value: "2026-11-01", rpc: new RpcClient(fetch, () => {}), context, change, run: () => {}, editor: { locale, popovers: messages.popovers, report, signal: scope.signal } });
  stops.push(() => scope.abort()); document.body.append(node);
  const editorResize = added.mock.calls.find(([type]) => type === "resize")?.[1]; expect(editorResize).toBeTypeOf("function");
  node.querySelector<HTMLButtonElement>('button[aria-haspopup="dialog"]')!.click(); expect(document.querySelector(".message-popover")).not.toBeNull();
  scope.abort(); expect(removed).toHaveBeenCalledWith("resize", editorResize); expect(document.querySelector(".message-popover")).toBeNull();
  const input = node.querySelector<HTMLInputElement>("input")!; input.value = "02/11/2026"; input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).not.toHaveBeenCalled();
});

test("disposable date adapter retains explicit cleanup without requiring an owner signal", () => {
  const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener"), change = vi.fn();
  const handle = createFieldControl({ field: field("scheduled", "date"), value: "2026-11-01", rpc: new RpcClient(fetch, () => {}), context, change, run: () => {}, editor: { locale, popovers: messages.popovers, report } });
  stops.push(handle.dispose); document.body.append(handle.node);
  const editorResize = added.mock.calls.find(([type]) => type === "resize")?.[1]; expect(editorResize).toBeTypeOf("function");
  handle.dispose(); expect(removed).toHaveBeenCalledWith("resize", editorResize);
  const input = handle.node.querySelector<HTMLInputElement>("input")!; input.value = "02/11/2026"; input.dispatchEvent(new Event("change", { bubbles: true })); expect(change).not.toHaveBeenCalled();
});

test("builtin editor invalid typing and owner abort never emit a new field value", () => {
  const scope = new AbortController(), change = vi.fn(), handle = fieldWidgets.get("date").render(environment(field("scheduled_date", "date"), "2026-11-01", change, scope.signal));
  stops.push(handle.dispose); document.body.append(handle.node); const input = handle.node.querySelector<HTMLInputElement>("input")!;
  input.value = "not a date"; input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).not.toHaveBeenCalled(); expect(input.value).toBe("01/11/2026");
  scope.abort(); input.value = "02/11/2026"; input.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).not.toHaveBeenCalled();
});

test("RecordForm forwards locale and scoped services to its widgets", () => {
  const target = field("probe", "char"), rpc = new RpcClient(fetch, () => {}), state = new RecordState(rpc, "test.event", [target], context), form = document.createElement("form"); document.body.append(form);
  const options = { form, state, view: view([target], "datetime_integration_probe"), rpc, context, messages, editorLocale: locale, run: () => {}, report, openRecord: async () => {}, change: () => {}, button: () => {} };
  const surface = new RecordForm(options); stops.push(() => surface.dispose()); surface.render();
  expect(probeEnvironments.at(-1)).toMatchObject({ editorLocale: locale, messages, context });
});

test("WizardDialog forwards locale without changing its record context", () => {
  const target = field("probe", "char"), rpc = new RpcClient(fetch, () => {}), state = new RecordState(rpc, "test.wizard", [target], context);
  const options = { name: "Event", state, view: view([target], "datetime_integration_probe"), rpc, context, messages, editorLocale: locale, report, notice: () => {}, openRecord: async () => {}, result: async () => {}, closed: async () => {} };
  const wizard = new WizardDialog(options); wizard.mount(); stops.push(() => wizard.dispose());
  expect(probeEnvironments.at(-1)).toMatchObject({ editorLocale: locale, messages, context });
});

test("related-record forms forward the same locale and shared popovers after metadata/defaults", async () => {
  const model = "test.event", target = field("probe", "char");
  const description = { wire_major: 1, registry_version: "r1", description_version: "d1", model, notices: [], fields: { [model]: { probe: { type: "char", label: "Probe", required: false, readonly: false, relation: null, relation_field: null, selection: null } } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "field", force_save: false, field: { model, name: "probe" }, widget: "datetime_integration_probe", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: target.label }] }] };
  const rpc = new RpcClient(async url => json(String(url).endsWith("get_views") ? description : String(url).endsWith("default_get") ? { probe: "" } : { value: {} }), () => {});
  const options = { dialogs: messages.dialogs, messages, editorLocale: locale, rpc, model, context, report };
  const result = editRelation(options);
  await vi.waitFor(() => expect(probeEnvironments.length).toBeGreaterThan(0));
  expect(probeEnvironments.at(-1)).toMatchObject({ editorLocale: locale, messages, context });
  messages.dialogs.dispose(); await result;
});

test("x2many line widgets inherit locale/services and abort without applying the line", async () => {
  const probe = field("probe", "char"), inverse = field("event_id", "many2one"), target: Field = { ...field("line_ids", "one2many"), relation: "test.line", relationField: "event_id", subview: { model: "test.line", list: [probe], form: view([probe], "datetime_integration_probe"), fields: new Map([[probe.name, probe], [inverse.name, inverse]]) } };
  const scope = new AbortController(), change = vi.fn(), handle = fieldWidgets.get("one2many").render(environment(target, [{ id: 1, probe: "Line" }], change, scope.signal));
  stops.push(handle.dispose); document.body.append(handle.node); handle.node.querySelector<HTMLElement>("tbody tr")!.click();
  await vi.waitFor(() => expect(probeEnvironments.length).toBeGreaterThan(0));
  expect(probeEnvironments.at(-1)).toMatchObject({ editorLocale: locale, messages, context });
  scope.abort(); expect(document.querySelector("dialog.x2many-dialog")).toBeNull(); expect(change).not.toHaveBeenCalled();
});

test("date and datetime edits traverse RecordState onchange/save as ISO and UTC values", async () => {
  const targets = [field("day", "date"), field("appointment", "datetime")], calls: { method: string; args: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (url, init) => {
    const method = String(url).split("/").at(-1)!, args = JSON.parse(String(init?.body)) as Record<string, unknown>; calls.push({ method, args });
    return json(method === "onchange" ? { value: {} } : method === "web_save" ? [{ id: 1, day: "2026-11-02", appointment: "2026-11-02 03:15:00" }] : [{ id: 1, day: "2026-11-01", appointment: "2026-11-01 09:00:00" }]);
  }, () => {});
  const state = new RecordState(rpc, "test.event", targets, context); await state.load(1);
  const form = document.createElement("form"); document.body.append(form); const jobs: Promise<void>[] = [];
  const options = { form, state, view: view(targets), rpc, context, messages, editorLocale: locale, run: () => {}, report, openRecord: async () => {}, change: (name: string, value: unknown) => { state.change(name, value); jobs.push(state.onchange(name)); }, button: () => {} };
  const surface = new RecordForm(options); stops.push(() => surface.dispose()); surface.render();
  const day = form.querySelector<HTMLInputElement>('input[name="day"]')!; expect(day.value).toBe("01/11/2026"); day.value = "02/11/2026"; day.dispatchEvent(new Event("change", { bubbles: true })); await Promise.all(jobs);
  const appointment = form.querySelector<HTMLInputElement>('input[name="appointment"]')!; appointment.value = "02/11/2026 10:15:00"; appointment.dispatchEvent(new Event("change", { bubbles: true })); await Promise.all(jobs);
  await state.save();
  expect(calls.filter(call => call.method === "onchange").map(call => call.args["values"])).toEqual([expect.objectContaining({ day: "2026-11-02" }), expect.objectContaining({ appointment: "2026-11-02 03:15:00" })]);
  expect(calls.find(call => call.method === "web_save")?.args).toMatchObject({ ids: [1], vals: { day: "2026-11-02", appointment: "2026-11-02 03:15:00" }, context });
  expect(errors).toEqual([]);
});

test("new date defaults, initial onchange, create and persisted readback keep the existing empty/UTC contracts", async () => {
  const targets = [field("day", "date"), field("appointment", "datetime")], calls: { method: string; args: Record<string, unknown> }[] = [];
  let stored: Record<string, unknown> = { day: "2026-11-01", appointment: "2026-11-01 09:00:00" };
  const rpc = new RpcClient(async (url, init) => {
    const method = String(url).split("/").at(-1)!, args = JSON.parse(String(init?.body)) as Record<string, unknown>; calls.push({ method, args });
    if (method === "default_get") return json(stored);
    if (method === "onchange") return json({ value: {} });
    if (method === "web_save") { stored = { id: 27, ...(args["vals"] as Record<string, unknown>) }; return json([stored]); }
    return json([stored]);
  }, () => {});
  const state = new RecordState(rpc, "test.event", targets, context); await state.load();
  expect(calls.slice(0, 2).map(call => call.method)).toEqual(["default_get", "onchange"]);
  expect(calls[1]!.args).toMatchObject({ ids: [], field_names: [], values: { day: "2026-11-01", appointment: "2026-11-01 09:00:00" }, context });
  const form = document.createElement("form"); document.body.append(form); const jobs: Promise<void>[] = [];
  const options = { form, state, view: view(targets), rpc, context, messages, editorLocale: locale, run: () => {}, report, openRecord: async () => {}, change: (name: string, value: unknown) => { state.change(name, value); jobs.push(state.onchange(name)); }, button: () => {} };
  const surface = new RecordForm(options); stops.push(() => surface.dispose()); surface.render();
  const day = form.querySelector<HTMLInputElement>('input[name="day"]')!; day.value = ""; day.dispatchEvent(new Event("change", { bubbles: true })); await Promise.all(jobs);
  expect(state.values["day"]).toBe(false);
  expect(calls.at(-1)).toMatchObject({ method: "onchange", args: { values: { day: null }, context } });
  const appointment = form.querySelector<HTMLInputElement>('input[name="appointment"]')!; appointment.value = "02/11/2026 10:15:00"; appointment.dispatchEvent(new Event("change", { bubbles: true })); await Promise.all(jobs);
  await state.save();
  expect(calls.find(call => call.method === "web_save")?.args).toMatchObject({ ids: [], vals: { day: null, appointment: "2026-11-02 03:15:00" }, context });
  expect(state.id).toBe(27); expect(state.dirty).toBe(false);
  await state.load(state.id); surface.render();
  expect(calls.at(-1)).toMatchObject({ method: "web_read", args: { ids: [27], context } });
  expect(form.querySelector<HTMLInputElement>('input[name="day"]')!.value).toBe("");
  expect(form.querySelector<HTMLInputElement>('input[name="appointment"]')!.value).toBe("02/11/2026 10:15:00");
  expect(errors).toEqual([]);
});

test("failed onchange/save retain the accepted date draft and propagate precise errors without replay", async () => {
  const target = field("day", "date"), calls: string[] = [];
  const rpc = new RpcClient(async url => {
    const method = String(url).split("/").at(-1)!; calls.push(method);
    if (method === "onchange") return new Response(JSON.stringify({ message: "Date onchange refused." }), { status: 422 });
    if (method === "web_save") return new Response(JSON.stringify({ message: "Date write refused." }), { status: 403 });
    return json([{ id: 1, day: "2026-11-01" }]);
  }, () => {});
  const state = new RecordState(rpc, "test.event", [target], context); await state.load(1);
  const form = document.createElement("form"); document.body.append(form); const jobs: Promise<void>[] = [];
  const options = { form, state, view: view([target]), rpc, context, messages, editorLocale: locale, run: () => {}, report, openRecord: async () => {}, change: (name: string, value: unknown) => { state.change(name, value); jobs.push(state.onchange(name).catch(report)); }, button: () => {} };
  const surface = new RecordForm(options); stops.push(() => surface.dispose()); surface.render();
  const input = form.querySelector<HTMLInputElement>("input")!; input.value = "02/11/2026"; input.dispatchEvent(new Event("change", { bubbles: true })); await Promise.all(jobs);
  expect(errors).toEqual([expect.objectContaining({ kind: "validation", message: "Date onchange refused." })]);
  expect(state.values["day"]).toBe("2026-11-02"); expect(state.dirty).toBe(true);
  await expect(state.save()).rejects.toMatchObject({ kind: "permission", message: "Date write refused." });
  expect(calls.filter(method => method === "web_save")).toHaveLength(1); expect(state.values["day"]).toBe("2026-11-02");
  state.discard(); surface.render(); expect(form.querySelector<HTMLInputElement>("input")!.value).toBe("01/11/2026"); expect(state.dirty).toBe(false);
});

test("HumanApp refuses missing declared locale formats rather than guessing them", () => {
  const host = document.createElement("div"); for (const region of ["header", "navigation", "notice", "content"]) { const child = document.createElement("div"); child.dataset[`client${region[0]!.toUpperCase()}${region.slice(1)}`] = ""; host.append(child); } document.body.append(host);
  const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true }, companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { lang: "en_GB", tz: "Asia/Jakarta" }, csrf: { logout: "test-token-not-secret" } };
  let app: HumanApp | undefined;
  try { expect(() => { app = new HumanApp(host, bootstrap); }).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid editor locale date_format." })); }
  finally { app?.dispose(); }
});


test("mixed RecordForm isolates unsupported date formats and keeps sibling changes usable", () => {
  const targets = [field("name", "char"), field("day", "date")], rpc = new RpcClient(fetch, () => {}), state = new RecordState(rpc, "test.event", targets, context), form = document.createElement("form");
  document.body.append(form); const change = vi.fn();
  const surface = new RecordForm({ form, state, view: view(targets), rpc, context, messages, editorLocale: { ...locale, date_format: "%j" }, run: () => {}, report, openRecord: async () => {}, change, button: () => {} });
  stops.push(() => surface.dispose()); surface.render();
  expect(form.textContent).toContain("Unsupported date/time format directive: %j.");
  const name = form.querySelector<HTMLInputElement>('input[name="name"]')!; expect(name.disabled).toBe(false);
  name.focus(); name.value = "Repair language"; name.dispatchEvent(new Event("change", { bubbles: true }));
  expect(change).toHaveBeenCalledWith("name", "Repair language", expect.anything());
  expect(errors).toEqual([]);
  expect(form.querySelector<HTMLInputElement>('input[name="day"]')!.disabled).toBe(true);
});

test("wizard forms retain sibling inputs when a date editor cannot use its configured format", () => {
  const targets = [field("name", "char"), field("day", "date")], rpc = new RpcClient(fetch, () => {}), state = new RecordState(rpc, "test.wizard", targets, context);
  const wizard = new WizardDialog({ name: "Event", state, view: view(targets), rpc, context, messages, editorLocale: { ...locale, date_format: "%j" }, report, notice: () => {}, openRecord: async () => {}, result: async () => {}, closed: async () => {} });
  wizard.mount(); stops.push(() => wizard.dispose());
  const dialog = document.querySelector("dialog")!;
  expect(dialog.textContent).toContain("Unsupported date/time format directive: %j.");
  expect(dialog.querySelector<HTMLInputElement>('input[name="name"]')!.disabled).toBe(false);
  expect(dialog.querySelector<HTMLInputElement>('input[name="day"]')!.disabled).toBe(true);
  expect(errors).toEqual([]);
});

test.each(["lang", "tz", "date_format", "time_format"] as const)("HumanApp still rejects malformed required locale member %s before shell startup", member => {
  for (const value of [undefined, "", 42, false]) {
    const root = document.createElement("div");
    const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true }, companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { ...locale, [member]: value }, csrf: { logout: "test-token-not-secret" } };
    expect(() => new HumanApp(root, bootstrap)).toThrowError(expect.objectContaining({ kind: "server", message: `Invalid editor locale ${member}.` }));
  }
});

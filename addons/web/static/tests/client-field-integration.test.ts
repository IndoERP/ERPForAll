/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import "../src/client/fields.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState, lineCommands } from "../src/client/record.js";
import type { Field, Subview, View } from "../src/client/metadata.js";
import { decodeAst, parseBoundedJson } from "../../../../web/packages/expr/src/index.js";
import { RpcClient } from "../src/client/transport.js";
const base = (name: string, kind: Field["kind"]): Field => ({ name, kind, label: name, readonly: false, required: false, selection: null, relation: null, relationField: null });
const view = (fields: Field[]): View => ({ type: "form", mode: "record", fields, nodes: [{ kind: "root", children: fields.map(field => ({ kind: "field", field, widget: field.widget ?? field.kind, label: field.label })) }] });
const nameExpression = decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [{ kind: "name", name: "locked" }] })));
afterEach(() => document.body.replaceChildren());
/// (partial: an unrelated form rerender preserves invalid scalar draft and save blocker)
test("invalid numeric draft survives an unrelated represented change", () => {
  const fields = [base("amount", "float"), base("name", "char")];
  const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
  const state = new RecordState(rpc, "fixture.record", fields, {}); state.values = { amount: 2, name: "Before" };
  const form = document.createElement("form"); document.body.append(form);
  const surface = new RecordForm({ form, state, view: view(fields), rpc, context: {}, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, run: job => { void job(); }, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render();
  const amount = form.querySelector<HTMLInputElement>('[name="amount"]')!; amount.value = "invalid"; amount.dispatchEvent(new Event("input"));
  expect(amount.validity.valid).toBe(false);
  state.change("name", "After"); surface.render();
  const restored = form.querySelector<HTMLInputElement>('[name="amount"]')!;
  expect(restored.value).toBe("invalid"); expect(restored.validity.valid).toBe(false); expect(surface.valid(false)).toBe(false);
  expect(state.values["amount"]).toBe(2); state.discard(); surface.render(); expect(form.querySelector<HTMLInputElement>('[name="amount"]')?.value).not.toBe("invalid"); surface.dispose();
});
/// (partial: parent serialization respects dynamic line policy in the line value scope)
test("parent line writes use effective readonly and required policy", () => {
  const locked = base("locked", "boolean"), title = { ...base("title", "char"), modifiers: { readonly: nameExpression, required: null, invisible: null } };
  const form = view([locked, title]); const subview: Subview = { model: "fixture.line", list: [], form, fields: new Map([[locked.name, locked], [title.name, title]]) };
  const field = { ...base("lines", "one2many"), subview };
  expect(lineCommands(field, [{ id: 1, locked: true, title: "Blocked edit", __changed: ["locked", "title"] }], [{ id: 1 }])).toEqual([[1, 1, { locked: true }]]);
  const required = { ...title, modifiers: { readonly: null, required: nameExpression, invisible: null } };
  subview.form = view([locked, required]);
  expect(() => lineCommands(field, [{ locked: true, title: false }], [])).toThrowError(expect.objectContaining({ kind: "validation", message: "title is required." }));
});

/// (partial: an asynchronous currency remount retains invalid draft after unrelated rerender)
test("invalid monetary draft survives asynchronous remount and cached currency delivery", async () => {
  const amount = { ...base("amount", "monetary"), widget: "monetary", options: { currency_field: { model: "fixture.record", name: "currency_id" } } };
  const currency = { ...base("currency_id", "many2one"), relation: "res.currency", readonly: true, invisible: true }, name = base("name", "char");
  const fields = [amount, currency, name];
  const rpc = new RpcClient(async () => new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }), () => {});
  const state = new RecordState(rpc, "fixture.record", fields, {}); state.values = { amount: 2, currency_id: { id: 1 }, name: "Before" };
  const form = document.createElement("form"); document.body.append(form);
  const surface = new RecordForm({ form, state, view: view([amount, name]), rpc, context: {}, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, run: job => { void job(); }, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render(); await new Promise(done => setTimeout(done, 0));
  const input = form.querySelector<HTMLInputElement>('[name="amount"]')!; input.value = "invalid"; input.dispatchEvent(new Event("input"));
  expect(input.validity.valid).toBe(false); state.change("name", "After"); surface.render();
  await new Promise(done => setTimeout(done, 0));
  const restored = form.querySelector<HTMLInputElement>('[name="amount"]')!;
  expect(restored.value).toBe("invalid"); expect(restored.validity.valid).toBe(false); expect(surface.valid(false)).toBe(false);
  restored.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })); state.change("name", "Again"); surface.render(); await new Promise(done => setTimeout(done, 0));
  expect(form.querySelector<HTMLInputElement>('[name="amount"]')?.value).toBe("2"); expect(surface.valid(false)).toBe(true); surface.dispose();
});

/// (partial: explicit float presentation on a monetary kind does not require currency data)
test("float adapter on a monetary field follows its explicit numeric presentation", async () => {
  const { resolveFieldWidget } = await import("../src/client/registry.js");
  const field = { ...base("amount", "monetary"), widget: "float", readonly: true };
  const rpc = new RpcClient(async () => { throw new Error("Unexpected currency RPC"); }, () => {});
  const handle = resolveFieldWidget("float", "list").render({ field, value: 2.5, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, rpc, context: {}, change: () => {}, run: () => {}, report: error => { throw error; } });
  expect(handle.node.textContent).toContain("2,5"); expect(handle.node.querySelector(".field-monetary")).toBeNull(); handle.dispose();
});

/// (partial: admitted domain/context RHS dependencies reach record reads and actual autocomplete)
test("relation expression dependencies without visible occurrences are loaded and evaluated", async () => {
  const { describeViews } = await import("../src/client/metadata.js");
  const { resolveFieldWidget } = await import("../src/client/registry.js");
  const domain = { schema: 1, nodes: [{ kind: "literal", value: { kind: "str", value: "currency_id" } }, { kind: "literal", value: { kind: "str", value: "!=" } }, { kind: "name", name: "id" }, { kind: "tuple", items: [0, 1, 2] }, { kind: "list", items: [3] }] };
  const context = { schema: 1, nodes: [{ kind: "literal", value: { kind: "str", value: "default_name" } }, { kind: "name", name: "name" }, { kind: "dict", items: [[0, 1]] }] };
  const descriptor = (type: string, readonly = false, relation: string | null = null) => ({ type, label: type, readonly, required: false, relation, relation_field: null, selection: null });
  const description = describeViews({ wire_major: 1, registry_version: "r1", description_version: "d1", model: "fixture.record", notices: [], fields: { "fixture.record": { id: descriptor("integer", true), name: descriptor("char"), company_id: descriptor("many2one", false, "res.company") } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "field", field: { model: "fixture.record", name: "company_id" }, widget: "many2one", options: { domain, context }, modifiers: { readonly: null, required: null, invisible: null }, force_save: false, label: "Company" }] }] }, "fixture.record", "r1");
  const form = description.views.get("form")!;
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push({ path: String(path), body: JSON.parse(String(init?.body)) as Record<string, unknown> });
    return new Response(JSON.stringify(String(path).endsWith("web_read") ? [{ id: 7, name: "Demo", company_id: false }] : []), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const state = new RecordState(rpc, "fixture.record", form.fields, {}); await state.load(7);
  expect(calls[0]?.body["specification"]).toMatchObject({ id: {}, name: {}, company_id: {} });
  const company = form.fields.find(field => field.name === "company_id")!;
  const { MessageServices } = await import("../src/client/messages.js");
  const messages = new MessageServices(document.body, error => { throw error; });
  const handle = resolveFieldWidget("many2one", "form").render({ messages, field: company, value: false, values: state.values, fields: state.fields, rpc, context: {}, change: () => {}, run: job => { void job(); }, report: error => { throw error; } });
  document.body.append(handle.node);
  const input = handle.node.querySelector<HTMLInputElement>("input")!; input.value = "Demo"; input.dispatchEvent(new Event("input"));
  await new Promise(done => setTimeout(done, 200));
  const search = calls.find(call => call.path.endsWith("web_name_search"));
  expect(search?.body).toMatchObject({ domain: [["currency_id", "!=", 7]], context: { default_name: "Demo" } }); handle.dispose(); messages.dispose();
});

/// (partial: successful reload invalidates drafts; a failed read preserves their owner epoch)
test("reload discards editor drafts only after a successful record read", async () => {
  const fields = [base("amount", "float")]; let fail = false;
  const rpc = new RpcClient(async () => fail ? new Response('{}', { status: 503, headers: { "Content-Type": "application/json" } }) : new Response('[{"id":7,"amount":2}]', { headers: { "Content-Type": "application/json" } }), () => {});
  const state = new RecordState(rpc, "fixture.record", fields, {}); await state.load(7);
  const form = document.createElement("form"); document.body.append(form);
  const surface = new RecordForm({ form, state, view: view(fields), rpc, context: {}, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, run: job => { void job(); }, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render(); const input = form.querySelector<HTMLInputElement>("input")!; input.value = "invalid"; input.dispatchEvent(new Event("input"));
  fail = true; const epoch = state.draftVersion; await expect(state.load(7)).rejects.toMatchObject({ kind: "server", message: "The request was refused. Please reload and try again." });
  expect(state.draftVersion).toBe(epoch); surface.render(); expect(form.querySelector<HTMLInputElement>("input")?.value).toBe("invalid");
  fail = false; await state.load(7); surface.render(); expect(form.querySelector<HTMLInputElement>("input")?.value).toBe("2"); expect(surface.valid(false)).toBe(true); surface.dispose();
});

/// (partial: unused large record relations are not copied into every autocomplete expression)
test("relation option evaluation bounds only the record inputs it actually references", async () => {
  const { relationQuery } = await import("../src/client/field-relations.js");
  const company = { ...base("company_id", "many2one"), relation: "res.company" };
  const values = { lines: Array.from({ length: 5000 }, (_, id) => ({ id })) };
  expect(relationQuery(company, values, {})).toEqual({ domain: [], context: {} });
  const document = { schema: 1, nodes: [{ kind: "name" as const, name: "lines" }] };
  expect(() => relationQuery({ ...company, options: { domain: document } }, values, {})).toThrowError(expect.objectContaining({ kind: "validation", message: "Unsupported relation expression value." }));
});

/// (partial: a field's constant context reaches the read specification, so variant tags show values only)
test("a relational field's constant context is sent with its read; a record-dependent one is not", async () => {
  const { specification } = await import("../src/client/record.js");
  const tags = { ...base("product_template_attribute_value_ids", "many2many"), relation: "product.template.attribute.value" };
  const constant = { schema: 1, nodes: [{ kind: "literal" as const, value: { kind: "str" as const, value: "display_attribute_name" } }, { kind: "literal" as const, value: { kind: "bool" as const, value: false } }, { kind: "dict" as const, items: [[0, 1]] as [number, number][] }] };
  expect(specification([{ ...tags, options: { context: constant } }])).toEqual({
    product_template_attribute_value_ids: { fields: { display_name: {} }, context: { display_attribute_name: false } },
  });
  expect(specification([tags])).toEqual({ product_template_attribute_value_ids: { fields: { display_name: {} } } });
  const dependent = { schema: 1, nodes: [{ kind: "literal" as const, value: { kind: "str" as const, value: "default_name" } }, { kind: "name" as const, name: "name" }, { kind: "dict" as const, items: [[0, 1]] as [number, number][] }] };
  expect(specification([{ ...tags, options: { context: dependent } }])).toEqual({ product_template_attribute_value_ids: { fields: { display_name: {} } } });
});

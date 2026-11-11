/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { describeViews, type Field, type Node } from "../src/client/metadata.js";
import { RecordState, lineCommands, lineFields } from "../src/client/record.js";
import { renderFormLayout } from "../src/client/form-layout.js";
import { ClientError, RpcClient } from "../src/client/transport.js";

const literal = (value: boolean) => ({ schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value } }] });
const node = (name: string, overrides: Record<string, unknown> = {}) => ({ kind: "field", field: { model: "test.note", name }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, force_save: false, label: name, ...overrides });
const modelField = (overrides: Record<string, unknown> = {}) => ({ type: "char", label: "Name", required: true, readonly: true, selection: null, relation: null, relation_field: null, ...overrides });
function fixture(form = [node("name")], list = [node("name")]) {
  return { wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields: { "test.note": { name: modelField(), hidden: modelField({ readonly: false }), id: modelField({ required: false }) } }, views: [
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: form.map((_, i) => i + 1) }, ...form] },
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: list.map((_, i) => i + 1) }, ...list] }
  ], notices: [] };
}
const parse = (value: unknown) => describeViews(value, "test.note", "r1");
const modifiers = (readonly: boolean | null, required: boolean | null, invisible: boolean | null) => ({ readonly: readonly === null ? null : literal(readonly), required: required === null ? null : literal(required), invisible: invisible === null ? null : literal(invisible) });

/// (partial: browser combined policy and per-occurrence flags; swapped node order not covered)
test("static overrides preserve model metadata and independent occurrence/view policies", () => {
  const description = parse(fixture([node("name", { modifiers: modifiers(false, false, true), force_save: true }), node("name") ]));
  expect(description.fields.get("name")).toMatchObject({ readonly: true, required: true });
  expect(description.views.get("list")!.fields[0]).toMatchObject({ readonly: true, required: true, invisible: false, forceSave: false });
  expect(description.views.get("form")!.fields).toHaveLength(1);
  // The served model readonly flag is a floor (HOST-03): an occurrence readonly=false cannot lift it.
  expect(description.views.get("form")!.fields[0]).toMatchObject({ readonly: true, required: true, invisible: false, forceSave: true });
  const root = description.views.get("form")!.nodes[0]!;
  if (root.kind !== "root") throw new Error("Expected root");
  expect(root.children[0]).toMatchObject({ field: { readonly: true, required: false, invisible: true, forceSave: true } });
  expect(root.children[1]).toMatchObject({ field: { readonly: true, required: true, invisible: false, forceSave: false } });
});

/// (partial: hidden control omitted and field retained; required skip and values are in the record tests)
test("hidden field nodes retain data policy but are omitted by the form layout", () => {
  const view = parse(fixture([node("hidden", { modifiers: modifiers(null, null, true) }), node("name")])).views.get("form")!;
  const host = document.createElement("div"), rendered: string[] = [];
  renderFormLayout(host, view.nodes, field => { rendered.push(field.field.name); }, new Map<Node, number>());
  expect(rendered).toEqual(["name"]);
  expect(view.fields.map(field => field.name)).toEqual(["hidden", "name"]);
});

const malformed: [string, (n: Record<string, unknown>) => void, string, string][] = [
  ["missing force_save", (n: Record<string, unknown>) => { delete n["force_save"]; }, "server", "Missing view node member: force_save."],
  ["wrong force_save", (n: Record<string, unknown>) => { n["force_save"] = 1; }, "server", "Invalid force_save."],
  ["nonboolean literal", (n: Record<string, unknown>) => { n["modifiers"] = { ...modifiers(null, null, null), required: { schema: 1, nodes: [{ kind: "literal", value: { kind: "int", value: "1" } }] } }; }, "unsupported", "Dynamic required modifiers is not supported by the current human UI slice."],
  ["missing modifier", (n: Record<string, unknown>) => { n["modifiers"] = { readonly: null, required: null }; }, "server", "Missing modifiers member: invisible."],
];

/// (partial: browser admission half)
test("rejects malformed field DTOs", () => {
  for (const [label, mutate, kind, message] of malformed) {
    const field = node("name"); mutate(field);
    try { parse(fixture([field])); throw new Error(`Expected rejection: ${label}`); }
    catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind, message }); }
  }
});

function recordFixture(create: boolean, forced: boolean) {
  const description = parse(fixture([node("name", { modifiers: modifiers(true, false, false), force_save: forced }), node("hidden", { modifiers: modifiers(false, true, true) }), node("id", { modifiers: modifiers(false, false, false), force_save: true })]));
  const calls: { method: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const method = String(path).split("/").at(-1)!;
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>; calls.push({ method, body });
    const value = method === "default_get" ? { name: "Default", hidden: "", id: 99 }
      : method === "onchange" ? { value: create ? {} : { name: "Calculated", hidden: "" } }
      : [{ id: 5, name: "Stored", hidden: "" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  return { state: new RecordState(rpc, "test.note", description.views.get("form")!.fields, {}), calls };
}

/// (partial: client serialization; live readback not run)
test("force_save readonly values persist on create and update while hidden required fields skip validation", async () => {
  for (const create of [true, false]) {
  const { state, calls } = recordFixture(create, true);
  await state.load(create ? undefined : 5);
  expect(() => state.change("name", "Forbidden")).toThrowError(new ClientError("validation", "This field cannot be edited."));
  expect(() => state.change("id", 100)).toThrowError(new ClientError("validation", "This field cannot be edited."));
  if (!create) await state.onchange("hidden");
  await state.save();
  expect(calls.at(-1)!.body["vals"]).toEqual({ name: create ? "Default" : "Calculated", hidden: "" });
  expect(calls.find(call => call.method === "onchange")!.body["values"]).toMatchObject({ name: create ? "Default" : "Stored" });
  }
});

/// (partial: client onchange and save serialization)
test("nonforced readonly values are omitted from save but included in onchange", async () => {
  for (const create of [true, false]) {
  const { state, calls } = recordFixture(create, false);
  await state.load(create ? undefined : 5);
  if (!create) await state.onchange("hidden");
  await state.save();
  expect(calls.at(-1)!.body["vals"]).toEqual({ hidden: "" });
  expect(calls.find(call => call.method === "onchange")!.body["values"]).toHaveProperty("name");
  }
});

/// (partial: line policy retained and line command; view switching not covered)
test("x2many line policies retain view overrides rather than replace them with model metadata", () => {
  const description = parse(fixture([node("name", { modifiers: modifiers(false, false, true), force_save: true })], []));
  const form = description.views.get("form")!;
  const subview = { model: "test.note", list: [], form, fields: description.fields };
  // The model field is readonly on the server, so the occurrence readonly=false stays readonly (floor).
  expect(lineFields(subview)[0]).toMatchObject({ readonly: true, required: false, invisible: true, forceSave: true });
  const field: Field = { name: "lines", kind: "one2many", label: "Lines", readonly: false, required: false, selection: null, relation: "test.note", relationField: "parent_id", subview };
  expect(lineCommands(field, [{ name: "New" }], [])).toEqual([[0, 0, { name: "New" }]]);
});

/// (partial: dynamic field modifiers are retained for record-snapshot evaluation)
test("dynamic modifier admission retains its validated expression", () => {
  const field = node("name", { modifiers: { readonly: { schema: 1, nodes: [{ kind: "name", name: "name" }] }, required: null, invisible: null } });
  const value = parse(fixture([field])).views.get("form")!;
  expect(value.fields[0]?.modifiers?.readonly?.nodes[0]).toMatchObject({ kind: "name", name: "name" });
});

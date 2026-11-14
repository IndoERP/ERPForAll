/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { RecordState } from "../src/client/record.js";
import { RpcClient } from "../src/client/transport.js";
import type { Field } from "../src/client/metadata.js";
const fields: Field[] = [{ name: "name", kind: "char", label: "Name", readonly: false, required: true, selection: null, relation: null, relationField: null }];
test("record defaults and onchange come from RPC and save uses actual values", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });
    const value = String(path).endsWith("default_get") ? { name: "Server default" }
      : String(path).endsWith("onchange") ? { value: { name: "Normalized" } } : [{ id: 14, name: "Normalized" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load();
  expect(state.values["name"]).toBe("Normalized");
  state.change("name", "Typed");
  await state.onchange("name");
  expect(state.dirty).toBe(true);
  await state.save();
  expect(state.id).toBe(14);
  expect(state.dirty).toBe(false);
  expect(calls.at(-1)?.body["vals"]).toEqual({ name: "Normalized" });
});
test("failed writes retain edits and block blind retry after ambiguous response", async () => {
  let writing = false;
  const rpc = new RpcClient(async () => {
    if (writing) throw new Error("Disconnected");
    return new Response('[{"id":5,"name":"Before"}]', { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load(5); state.change("name", "After"); writing = true;
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(state.values["name"]).toBe("After"); expect(state.dirty).toBe(true);
  await expect(state.save()).rejects.toThrow("Reload the record before another write");
});

test("failed reload after an ambiguous create retains edits and blocks another create", async () => {
  let defaults = 0, writes = 0;
  const rpc = new RpcClient(async path => {
    if (String(path).endsWith("default_get")) {
      defaults++;
      if (defaults > 1) throw new Error("Reload unavailable");
      return new Response('{"name":"Draft"}', { headers: { "Content-Type": "application/json" } });
    }
    if (String(path).endsWith("onchange")) return new Response('{"value":{}}', { headers: { "Content-Type": "application/json" } });
    writes++;
    throw new Error("Create response lost");
  }, () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load(); state.change("name", "Entered name");
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(state.load()).rejects.toMatchObject({ kind: "network" });
  expect(state.id).toBeUndefined();
  expect(state.values["name"]).toBe("Entered name");
  expect(state.dirty).toBe(true);
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(writes).toBe(1);
});

test("successful defaults do not resolve an ambiguous create outcome", async () => {
  let defaults = 0, writes = 0;
  const rpc = new RpcClient(async path => {
    if (String(path).endsWith("default_get")) {
      defaults++;
      return new Response(JSON.stringify({ name: defaults === 1 ? "Draft" : "Other default" }), { headers: { "Content-Type": "application/json" } });
    }
    if (String(path).endsWith("onchange")) return new Response('{"value":{}}', { headers: { "Content-Type": "application/json" } });
    writes++;
    throw new Error("Create response lost");
  }, () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load(); state.change("name", "Entered name");
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(state.load()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(state.values["name"]).toBe("Entered name");
  expect(state.dirty).toBe(true);
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(writes).toBe(1);
});

test("failed authoritative reload retains an existing record's ambiguous edits", async () => {
  let writing = false;
  const rpc = new RpcClient(async () => {
    if (writing) throw new Error("Disconnected");
    return new Response('[{"id":5,"name":"Before"}]', { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load(5); state.change("name", "After"); writing = true;
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(state.load()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(state.load(5)).rejects.toMatchObject({ kind: "network" });
  expect(state.id).toBe(5);
  expect(state.values["name"]).toBe("After");
  expect(state.dirty).toBe(true);
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
});

test("malformed successful save is ambiguous and cannot be replayed", async () => {
  let writes = 0;
  const rpc = new RpcClient(async path => new Response(JSON.stringify(String(path).endsWith("web_save") ? (++writes, [{ id: "invalid", name: "Changed" }]) : [{ id: 5, name: "Before" }]), { headers: { "Content-Type": "application/json" } }), () => {});
  const state = new RecordState(rpc, "test.note", fields, {});
  await state.load(5); state.change("name", "Changed");
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(state.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(writes).toBe(1); expect(state.values["name"]).toBe("Changed");
});

test("many2many onchange link diffs become projected choices and writable SET commands", async () => {
  const relation: Field = { name: "groups_id", kind: "many2many", label: "Groups", readonly: false, required: false, relation: "res.groups", relationField: null, selection: null };
  const requests: Record<string, unknown>[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>; requests.push(args);
    const value = String(path).endsWith("default_get") ? { groups_id: [] } : String(path).endsWith("onchange") ? { value: { groups_id: [[4, 8, { id: 8, display_name: "Operator" }]] } } : [{ id: 1, groups_id: [{ id: 8, display_name: "Operator" }] }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const state = new RecordState(rpc, "res.users", [relation], {}); await state.load();
  expect(state.values["groups_id"]).toEqual([{ id: 8, display_name: "Operator" }]); await state.save();
  expect(requests.at(-1)?.["vals"]).toEqual({ groups_id: [[6, 0, [8]]] });
});

const defaultGroups: Field = { name: "group_ids", kind: "many2many", label: "Groups", readonly: false, required: false, relation: "res.groups", relationField: null, selection: null };
test.each([
  { defaults: [[6, 0, [1, 2]]], expected: [1, 2] },
  { defaults: [[4, 1, 0], [4, 2, 0]], expected: [1, 2] },
  { defaults: [[6, 0, [1]], [5, 0, 0], [4, 2, 0]], expected: [2] },
  { defaults: [1, 2], expected: [1, 2] },
  { defaults: false, expected: [] },
])("new forms normalize relation defaults $defaults before onchange and save", async ({ defaults, expected }) => {
  const calls: { path: string; args: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push({ path: String(path), args: JSON.parse(String(init?.body)) });
    const value = String(path).endsWith("default_get") ? { name: "New user", group_ids: defaults }
      : String(path).endsWith("onchange") ? { value: {} }
      : [{ id: 17, name: "New user", group_ids: expected }];
    return new Response(JSON.stringify(value));
  }, () => {});
  const state = new RecordState(rpc, "res.users", [...fields, defaultGroups], {});
  await state.load();
  expect(state.values["group_ids"]).toEqual(expected);
  expect(calls[1]?.args["values"]).toEqual({ name: "New user", group_ids: [[6, 0, expected]] });
  expect(state.dirty).toBe(false);
  await state.save();
  expect(calls[2]?.args["vals"]).toEqual({ name: "New user", group_ids: [[6, 0, expected]] });
  expect(state.id).toBe(17);
});
test("malformed relation defaults fail before onchange or create and retain the previous record", async () => {
  const calls: string[] = [];
  const rpc = new RpcClient(async path => {
    calls.push(String(path));
    return new Response(JSON.stringify(String(path).endsWith("default_get") ? { group_ids: [[6, 0, ["bad-id"]]] } : [{ id: 5, name: "Existing", group_ids: [] }]));
  }, () => {});
  const state = new RecordState(rpc, "res.users", [...fields, defaultGroups], {});
  await state.load(5);
  await expect(state.load()).rejects.toMatchObject({ kind: "server" });
  expect(state.id).toBe(5); expect(state.values["name"]).toBe("Existing");
  expect(calls).toHaveLength(2);
});

test("the first onchange of a new form replaces its one2many default lines instead of appending to them", async () => {
  // The first onchange answers the complete value (every line as a create), not a diff against
  // the defaults the client sent: a new tax form showed each default distribution line twice.
  const kind: Field = { name: "repartition_type", kind: "char", label: "Type", readonly: false, required: true, selection: null, relation: null, relationField: null };
  const subview = { model: "test.line", list: [kind], form: undefined, fields: new Map([[kind.name, kind]]) };
  const lineField: Field = { name: "line_ids", kind: "one2many", label: "Lines", readonly: false, required: false, selection: null, relation: "test.line", relationField: "parent_id", subview };
  const calls: { path: string; args: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push({ path: String(path), args: JSON.parse(String(init?.body)) });
    const value = String(path).endsWith("default_get") ? { line_ids: [[0, 0, { repartition_type: "base" }], [0, 0, { repartition_type: "tax" }]] }
      : String(path).endsWith("onchange") ? { value: { line_ids: [[0, "virtual_1", { repartition_type: "base", sequence: 1 }], [0, "virtual_2", { repartition_type: "tax", sequence: 1 }]] } }
      : [{ id: 3, line_ids: [] }];
    return new Response(JSON.stringify(value));
  }, () => {});
  const state = new RecordState(rpc, "test.parent", [lineField], {});
  await state.load();
  expect(calls[1]?.args["values"]).toEqual({ line_ids: [[0, 0, { repartition_type: "base" }], [0, 0, { repartition_type: "tax" }]] });
  expect((state.values["line_ids"] as Record<string, unknown>[]).map(line => [line["repartition_type"], line["sequence"]])).toEqual([["base", 1], ["tax", 1]]);
  expect(state.dirty).toBe(false);
  await state.save();
  expect(calls[2]?.args["vals"]).toEqual({ line_ids: [[0, 0, { repartition_type: "base" }], [0, 0, { repartition_type: "tax" }]] });
});

test.each(["many2one", "char","text", "selection", "date", "datetime", "integer", "float", "boolean"] as const)("empty %s uses the RPC null sentinel without changing boolean false", async kind => {
  const field: Field = { name: "optional", kind, label: "Optional", readonly: false, required: false, relation: kind === "many2one" ? "res.company" : null, relationField: null, selection: kind === "selection" ? [["a", "A"]] : null };
  const calls: Record<string, unknown>[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push(JSON.parse(String(init?.body)));
    const value = String(path).endsWith("default_get") ? { optional: false }
      : String(path).endsWith("onchange") ? { value: {} } : [{ id: 4, optional: false }];
    return new Response(JSON.stringify(value));
  }, () => {});
  const state = new RecordState(rpc, "test.note", [field], {});await state.load();
  expect(calls[1]?.["values"]).toEqual({ optional: kind === "boolean" ? false : null });
  await state.save();expect(calls[2]?.["vals"]).toEqual({ optional: kind === "boolean" ? false : null });
});

/// (partial: requested absent placeholder defaults remain canonical record omissions)
test("record defaults preserve absent and malformed placeholder data without synthetic writes", async () => {
  const name: Field = { ...fields[0]!, options: { placeholder_field: { model: "test.note", name: "hint" } } };
  const hint: Field = { ...fields[0]!, name: "hint", label: "Hint", readonly: true, required: false };
  let defaults: Record<string, unknown> = {};
  const rpc = new RpcClient(async path => new Response(JSON.stringify(String(path).endsWith("default_get") ? defaults : { value: {} }), { headers: { "Content-Type": "application/json" } }), () => {});
  const state = new RecordState(rpc, "test.note", [name, hint], {}); await state.load();
  expect(Object.hasOwn(state.values, "hint")).toBe(false); expect(Object.hasOwn(state.values, "name")).toBe(false); expect(state.dirty).toBe(false);
  defaults = { hint: 42 }; await state.load(); expect(state.values["hint"]).toBe(42);
});

test("editable related field shows typed value right after web_save and normalized value upon reload", async () => {
  const companyFields: Field[] = [
    { name: "name", kind: "char", label: "Name", readonly: false, required: true, relation: null, relationField: null, selection: null },
    { name: "website", kind: "char", label: "Website", readonly: false, required: false, relation: null, relationField: null, selection: null },
  ];
  let savedWebsite = "http://acme.example";
  const rpc = new RpcClient(async (path, init) => {
    const p = String(path);
    if (p.endsWith("web_save")) {
      const body = JSON.parse(String(init?.body)) as { vals?: Record<string, unknown> };
      const typedWebsite = body.vals?.["website"] as string;
      return new Response(JSON.stringify([{ id: 1, name: "Acme", website: typedWebsite }]), {
        headers: { "Content-Type": "application/json" },
      });
    }
    return new Response(JSON.stringify([{ id: 1, name: "Acme", website: savedWebsite }]), {
      headers: { "Content-Type": "application/json" },
    });
  }, () => {});

  const state = new RecordState(rpc, "res.company", companyFields, {});
  await state.load(1);
  expect(state.values["website"]).toBe("http://acme.example");

  state.change("website", "sync.example");
  expect(state.dirty).toBe(true);
  await state.save();

  expect(state.dirty).toBe(false);
  expect(state.values["website"]).toBe("sync.example");

  savedWebsite = "http://sync.example";
  await state.load(1);
  expect(state.values["website"]).toBe("http://sync.example");
});


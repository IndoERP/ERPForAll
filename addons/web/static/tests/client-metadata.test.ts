/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import searchHost from "./fixtures/h18/search-host.json";
import readonlyFieldHost from "./fixtures/h18/readonly-field-host.json";
import readonlyModelHost from "./fixtures/h18/readonly-model-host.json";
import type { ViewDescription } from "../src/generated/human-ui.js";
import { describeViews } from "../src/client/metadata.js";

const fixture = () => ({ wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields: { "test.note": { name: { type: "char", label: "Name", required: true, readonly: false, selection: null, relation: null, relation_field: null } } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }] }], notices: [] } satisfies ViewDescription);
test("descriptions resolve actual fields and preserve required metadata", () => {
  const view = describeViews(fixture(), "test.note", "r1");
  expect(view.model).toBe("test.note");
  expect(view.fields.get("name")).toMatchObject({ kind: "char", required: true });
});
test("unknown fields, renderers, AST behavior and node cycles fail explicitly", () => {
  const missing = fixture(); missing.views[0]!.nodes[1]!.field!.name = "password_hash";
  expect(() => describeViews(missing, "test.note", "r1")).toThrow("Unknown field in view: password_hash.");
  const renderer = fixture(); renderer.views[0]!.renderer = "custom.magic";
  expect(() => describeViews(renderer, "test.note", "r1")).toThrow("Unsupported view renderer: custom.magic.");
  const cyclic = fixture(); cyclic.views[0]!.nodes[0]!.children = [0];
  expect(() => describeViews(cyclic, "test.note", "r1")).toThrow("View structure contains a cycle or repeated node.");
  expect(() => describeViews(fixture(), "test.note", "r2")).toThrow("The application changed. Reload to use the current version.");
});

test("description admission rejects unknown semantic members and malformed view identities", () => {
  const unknown = fixture();
  Object.assign(unknown.views[0]!.nodes[0]!, { invisible: true });
  expect(() => describeViews(unknown, "test.note", "r1")).toThrow("Unknown view node member: invisible.");
  const identity = fixture(); Object.assign(identity.views[0]!, { identity: { kind: "stored", id: 0 } });
  expect(() => describeViews(identity, "test.note", "r1")).toThrow("Invalid stored view identity.");
  const expression = fixture(); Object.assign(expression.views[0]!.nodes[1]!.modifiers!, { invisible: { schema: 1 } });
  expect(() => describeViews(expression, "test.note", "r1")).toThrow();
});

const structured = () => {
  const d=fixture();
  Object.assign(d.views[0]!, { nodes: [
    {kind:"form",record_mode:"record",children:[1]},
    {kind:"sheet",children:[2,3]},
    {kind:"group",label:"Details",columns:2,colspan:2,children:[6]},
    {kind:"notebook",children:[4]},
    {kind:"page",name:"details",label:"Details",children:[5]},
    {kind:"separator",label:"More details"},
    d.views[0]!.nodes[1]
  ] });
  return d;
};
/// (partial: consumer preserves authorized typed structure)
test("form metadata retains sheets, labelled groups, pages and separators",()=>{
  const parsed=describeViews(structured(),"test.note","r1");
  expect(parsed.views.get("form")!.nodes[0]).toMatchObject({kind:"root",children:[{kind:"sheet",children:[{kind:"group",label:"Details",columns:2}, {kind:"notebook",children:[{kind:"page",name:"details",label:"Details"}]}]}]});
  expect(parsed.views.get("form")!.fields.map(f=>f.name)).toEqual(["name"]);
});
/// (partial: consumer refuses misplaced pages, non-page children and unsafe column vocabulary)
test("malformed form layout cannot silently flatten or ignore semantic attributes",()=>{
  const misplaced=structured();Object.assign(misplaced.views[0]!.nodes[1]!,{children:[4]});
  expect(()=>describeViews(misplaced,"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  const wrongChild=structured();Object.assign(wrongChild.views[0]!.nodes[3]!,{children:[5]});
  expect(()=>describeViews(wrongChild,"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  const odd=structured();Object.assign(odd.views[0]!.nodes[2]!,{columns:3});
  expect(()=>describeViews(odd,"test.note","r1")).toThrow("Unsupported group columns or colspan.");
  const unknown=structured();Object.assign(unknown.views[0]!.nodes[2]!,{magic:true});
  expect(()=>describeViews(unknown,"test.note","r1")).toThrow("Unknown view node member: magic.");
});

/// (partial: consumer enforces 4096 aggregate nodes across returned views)
test("metadata rejects aggregate arena overflow even when every view fits",()=>{
 const d=fixture();
 const makeNodes=(kind:string,count:number)=>[
  {kind,...(kind==="form"?{record_mode:"record"}:{}),children:Array.from({length:count-1},(_,i)=>i+1)},
  ...Array.from({length:count-1},()=>d.views[0]!.nodes[1]!)
 ];
 Object.assign(d.views[0]!,{nodes:makeNodes("form",2048)});
 const list={identity:{kind:"stored",id:2},type:"list",schema_major:1,renderer:"web.list",root:0,nodes:makeNodes("list",2049)};
 Object.assign(d,{views:[d.views[0],list]});
 expect(()=>describeViews(d,"test.note","r1")).toThrow("Requested views exceed the 4096-node aggregate limit.");
});

/// (partial: client admits only a single root-level aside)
test("form aside preserves declared fields and refuses invalid placement", () => {
  const d=fixture();
  Object.assign(d.views[0]!, {nodes:[
    {kind:"form",record_mode:"record",children:[1,2]},
    {kind:"sheet",children:[]}, {kind:"aside",children:[3]},
    {kind:"group",label:"Record information",columns:2,colspan:2,children:[4]},
    d.views[0]!.nodes[1]
  ]});
  expect(describeViews(d,"test.note","r1").views.get("form")!.nodes[0]).toMatchObject({kind:"root",children:[{kind:"sheet"},{kind:"aside",children:[{kind:"group",label:"Record information"}]}]});
  const nested=structured(); Object.assign(nested.views[0]!, { nodes: nested.views[0]!.nodes.map((node, index) => index === 2 ? {kind:"aside",children:[6]} : node) });
  expect(()=>describeViews(nested,"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  const duplicate=structured(); Object.assign(duplicate.views[0]!, {nodes:[
    {kind:"form",record_mode:"record",children:[1,2]}, {kind:"aside",children:[]}, {kind:"aside",children:[]}
  ]});
  expect(()=>describeViews(duplicate,"test.note","r1")).toThrow("A record form allows at most one aside.");
});

const chatterNode = (extra: Record<string, unknown> = {}) => ({kind:"chatter",reload_on_post:true,reload_on_attachment:false,reload_on_activity:false,reload_on_follower:true,open_attachments:false,has_activities:true,...extra});

/// (partial: client decodes the closed chatter node and refuses bad shape and placement)
test("form chatter decodes all six flags and refuses invalid shape and placement", () => {
  const d=fixture();
  Object.assign(d.views[0]!, {nodes:[
    {kind:"form",record_mode:"record",children:[1,2,3]}, {kind:"sheet",children:[]}, {kind:"aside",children:[]}, chatterNode()
  ]});
  expect(describeViews(d,"test.note","r1").views.get("form")!.nodes[0]).toMatchObject({kind:"root",children:[{kind:"sheet"},{kind:"aside"},
    {kind:"chatter",reloadOnPost:true,reloadOnAttachment:false,reloadOnActivity:false,reloadOnFollower:true,openAttachments:false,hasActivities:true}]});
  const build=(nodes:unknown[])=>{const v=fixture();Object.assign(v.views[0]!,{nodes});return v;};
  const form=(children:number[],mode="record")=>({kind:"form",record_mode:mode,children});
  expect(()=>describeViews(build([form([1,2]),chatterNode(),chatterNode()]),"test.note","r1")).toThrow("A record form allows at most one chatter.");
  expect(()=>describeViews(build([form([1]),{kind:"sheet",children:[2]},chatterNode()]),"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  expect(()=>describeViews(build([form([1]),{kind:"aside",children:[2]},chatterNode()]),"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  expect(()=>describeViews(build([form([1],"service"),chatterNode()]),"test.note","r1")).toThrow("Invalid form layout hierarchy.");
  expect(()=>describeViews(build([form([1]),chatterNode({children:[]})]),"test.note","r1")).toThrow("Unknown view node member: children.");
  const missing:Record<string,unknown>=chatterNode();delete missing["has_activities"];
  expect(()=>describeViews(build([form([1]),missing]),"test.note","r1")).toThrow("Missing view node member: has_activities.");
  expect(()=>describeViews(build([form([1]),chatterNode({reload_on_post:"1"})]),"test.note","r1")).toThrow("Invalid chatter reload_on_post.");
});

/// (partial: strict title consumer, fields still use ordinary authorization metadata)
test("explicit title headings retain field policy and reject arbitrary markup", () => {
  const d = fixture(), original = d.views[0]!.nodes[1]!;
  Object.assign(d.views[0]!, { nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "sheet", children: [2] },
    { kind: "title", children: [3] },
    { kind: "heading", level: 1, children: [4] }, original
  ] });
  const parsed = describeViews(d, "test.note", "r1");
  expect(parsed.views.get("form")!.fields[0]).toMatchObject({ name: "name", required: true });
  expect(parsed.views.get("form")!.nodes[0]).toMatchObject({ kind: "root", children: [{ kind: "sheet", children: [{ kind: "title", children: [{ kind: "heading", level: 1 }] }] }] });
  const badLevel = structuredClone(d); Object.assign(badLevel.views[0]!.nodes[3]!, { level: 3 });
  expect(() => describeViews(badLevel, "test.note", "r1")).toThrow("Only title heading levels 1 and 2");
  const badParent = structuredClone(d); Object.assign(badParent.views[0]!.nodes[1]!, { children: [3] });
  expect(() => describeViews(badParent, "test.note", "r1")).toThrow("Invalid form layout hierarchy");
  const arbitrary = structuredClone(d); Object.assign(arbitrary.views[0]!.nodes[2]!, { html: "<b>unsafe</b>" });
  expect(() => describeViews(arbitrary, "test.note", "r1")).toThrow("Unknown view node member: html");
});

function smartDescription(): unknown {
  const d = fixture();
  Object.assign(d.fields["test.note"], { count: { type: "integer", label: "Lines", required: false, readonly: true, selection: null, relation: null, relation_field: null } });
  Object.assign(d.views[0]!, { nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "sheet", children: [2] },
    { kind: "button_box", children: [3] },
    { kind: "stat_button", name: "open_lines", button_type: "object", label: "Lines", variant: "secondary", confirm: null, modifiers: { readonly: null, required: null, invisible: null }, children: [4] },
    { kind: "field", force_save: false, field: { model: "test.note", name: "count" }, widget: "statinfo", label: "Lines", options: {}, modifiers: { readonly: null, required: null, invisible: null } }
  ] });
  return d;
}
/// (partial: strict typed consumer; server inheritance/RPC tested separately)
test("smart buttons retain their XML sheet placement and numeric readonly fields", () => {
  const data = smartDescription() as ReturnType<typeof fixture>;
  const parsed = describeViews(data, "test.note", "r1");
  expect(parsed.views.get("form")!.nodes[0]).toMatchObject({kind:"root", children:[{kind:"sheet",children:[{kind:"button_box",children:[{kind:"stat_button",name:"open_lines",children:[{kind:"field",widget:"statinfo"}]}]}]}]});
  expect(parsed.views.get("form")!.fields).toMatchObject([{name:"count",readonly:true}]);
  Object.assign(data.views[0]!.nodes[2]!, {children:[4]});
  expect(() => describeViews(data,"test.note","r1")).toThrow("Invalid form layout hierarchy.");
});

/// (partial: explicit list filter metadata, no inferred workflow semantics)
test("list tabs require a real declared selection field and reject missing or nonselection references", () => {
  const data = fixture();
  Object.assign(data.fields["test.note"], { priority: { type: "selection", label: "Priority", required: false, readonly: false, selection: [["normal", "Normal"], ["high", "High"]], relation: null, relation_field: null } });
  Object.assign(data.views[0]!, { type: "list", renderer: "web.list", nodes: [
    { kind: "list", children: [1], search_panel: { kind: "selection", field: { model: "test.note", name: "priority" }, label: "Priority", enable_counters: true, expand: true, default_value: null, notice: null } },
    data.views[0]!.nodes[1]
  ] });
  const parsed = describeViews(data, "test.note", "r1");
  expect(parsed.views.get("list")?.searchPanel).toMatchObject({ kind: "selection", field: { name: "priority", kind: "selection" } });
  expect(parsed.views.get("list")?.fields.map(field => field.name)).toEqual(["name"]);
  const root = data.views[0]!.nodes[0]!;
  Object.assign((root as unknown as { search_panel: object }).search_panel, { field: { model: "test.note", name: "missing" } });
  expect(() => describeViews(data, "test.note", "r1")).toThrow("Unknown search panel field: missing.");
  Object.assign((root as unknown as { search_panel: object }).search_panel, { field: { model: "test.note", name: "name" } });
  expect(() => describeViews(data, "test.note", "r1")).toThrow("List tabs require 1–32 declared selection choices.");
  Object.assign((root as unknown as { search_panel: object }).search_panel, { field: { model: "other.note", name: "priority" } });
  expect(() => describeViews(data, "test.note", "r1")).toThrow("Unknown search panel field: priority.");
});

/// (partial: legacy list defaults and explicit custom page admission)
test("list page limits are explicit, bounded and absent on unchanged legacy descriptions", () => {
  const data = fixture();
  Object.assign(data.views[0]!, { type: "list", renderer: "web.list", nodes: [{ kind: "list", children: [1], page_limit: 75 }, data.views[0]!.nodes[1]] });
  expect(describeViews(data, "test.note", "r1").views.get("list")?.pageLimit).toBe(75);
  Object.assign(data.views[0]!.nodes[0]!, { page_limit: 0 });
  expect(() => describeViews(data, "test.note", "r1")).toThrow("List page limit must be a positive browser-safe integer.");
});


test("standalone Search decodes its typed selection panel without widening references", () => {
  const data = fixture();
  Object.assign(data.fields["test.note"], { priority: { type: "selection", label: "Priority", required: false, readonly: false, selection: [["normal", "Normal"]], relation: null, relation_field: null } });
  const panel = { kind: "selection", field: { model: "test.note", name: "priority" }, label: "Priority", enable_counters: false, expand: true, default_value: null, notice: null };
  Object.assign(data.views[0]!, { type: "search", renderer: "web.search", nodes: [{ kind: "search", children: [], search_panel: panel }] });
  expect(describeViews(data, "test.note", "r1").views.get("search")?.searchPanel).toMatchObject({ kind: "selection", field: { name: "priority" }, expand: true });
  panel.field.name = "internal_memo";
  expect(() => describeViews(data, "test.note", "r1")).toThrow("Unknown search panel field: internal_memo.");
});

const boolExpression = (value: boolean) => ({ schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value } }] });
const searchLeaves = (present: boolean) => {
  const data = fixture();
  Object.assign(data.views[0]!, { type: "search", renderer: "web.search", nodes: [
    { kind: "search", children: [1, 2, 5] },
    { kind: "search_field", field: { model: "test.note", name: "name" }, label: "Name", ...(present ? { filter_domain: boolExpression(true), operator: "ilike" } : {}) },
    { kind: "search_group", label: "Filters", expand: true, children: [3, 4] },
    { kind: "search_filter", name: "named", label: "Named", ...(present ? { domain: boolExpression(true), context: boolExpression(false) } : {}) },
    { kind: "search_separator" }, { kind: "search_separator" },
  ] });
  const parsed = describeViews(data, "test.note", "r1").views.get("search")!;
  expect(parsed.nodes[0]).toMatchObject({ kind: "root", children: [
    { kind: "search_field", operator: present ? "ilike" : null, filterDomain: present ? expect.any(Object) : null },
    { kind: "search_group", expand: true, children: [{ kind: "search_filter", domain: present ? expect.any(Object) : null, context: present ? expect.any(Object) : null }, { kind: "search_separator" }] },
    { kind: "search_separator" },
  ] });
};
/// (partial: nonempty Search consumer leaf/group and optional expression contracts)
test("Search leaves decode before container children; optional slots absent", () => searchLeaves(false));
/// (partial: nonempty Search consumer leaf/group and optional expression contracts)
test("Search leaves decode before container children; optional slots present", () => searchLeaves(true));
const readonlyFloor = (floor: boolean) => {
  const data = fixture();
  Object.assign(data.fields["test.note"].name, { readonly: floor });
  const original = data.views[0]!.nodes[1]!;
  Object.assign(data.views[0]!, { nodes: [
    { kind: "form", record_mode: "record", children: [1, 2] },
    { ...original, modifiers: { readonly: boolExpression(true), required: null, invisible: null } },
    { ...original, modifiers: { readonly: boolExpression(false), required: null, invisible: null } },
  ] });
  const view = describeViews(data, "test.note", "r1").views.get("form")!;
  expect(view.nodes[0]).toMatchObject({ kind: "root", children: [{ field: { readonly: true } }, { field: { readonly: floor } }] });
  expect(view.fields.map(field => field.readonly)).toEqual([floor]);
};
/// (partial: readonly metadata is a floor across repeated XML occurrences)
test("effective metadata readonly=false remains authoritative", () => readonlyFloor(false));
/// (partial: readonly metadata is a floor across repeated XML occurrences)
test("effective metadata readonly=true remains authoritative", () => readonlyFloor(true));

/// (partial: client accepts the actual SearchModel subset and refuses other text)
test("search field operator contract is explicit", () => {
  for (const operator of ["ilike", "=", "!=", ">", "<", ">=", "<=", "unknown_operator"]) {
    const data = fixture();
    Object.assign(data.views[0]!, { type: "search", renderer: "web.search", nodes: [
      { kind: "search", children: [1] },
      { kind: "search_field", field: { model: "test.note", name: "name" }, label: "Name", operator },
    ] });
    const decode = () => describeViews(data, "test.note", "r1");
    if (operator === "unknown_operator") {
      expect(decode).toThrow("Unsupported search field operator 'unknown_operator' for 'test.note.name'.");
      try { decode(); } catch (error) { expect(error).toMatchObject({ kind: "unsupported", message: "Unsupported search field operator 'unknown_operator' for 'test.note.name'." }); }
    }
    else expect(decode().views.get("search")!.nodes[0]).toMatchObject({ children: [{ operator }] });
  }
});

function decodeHostCapture(raw: unknown, model: string) {
  if (raw === null || typeof raw !== "object" || !("registry_version" in raw) || typeof raw.registry_version !== "string") throw new Error("Invalid authenticated host capture.");
  return describeViews(raw, model, raw.registry_version);
}
/// (partial: actual authenticated host Search capture reaches the client decoder)
test("actual host Search capture includes leaves, groups, expressions and panel", () => {
  const view = decodeHostCapture(searchHost, "acceptance_notes.note").views.get("search")!;
  expect(view.nodes[0]).toMatchObject({ kind: "root", children: [
    { kind: "search_field", operator: "ilike", filterDomain: expect.any(Object) },
    { kind: "search_filter", domain: expect.any(Object) }, { kind: "search_separator" },
    { kind: "search_group", expand: true, children: [{ kind: "search_filter", context: expect.any(Object) }, { kind: "search_separator" }] },
  ] });
  expect(view.searchPanel).toMatchObject({ kind: "selection", field: { name: "priority" } });
});
/// (partial: real model/field write-mask metadata survives occurrence overrides)
test("actual host readonly captures retain the access floor in client state", () => {
  const model = decodeHostCapture(readonlyModelHost, "acceptance_notes.note").views.get("form")!;
  expect(model.fields).toMatchObject([{ name: "name", readonly: true }]);
  expect(model.nodes[0]).toMatchObject({ children: [{ field: { readonly: true } }, { field: { readonly: true } }] });
  const field = decodeHostCapture(readonlyFieldHost, "res.users").views.get("form")!;
  expect(field.fields).toMatchObject([{ name: "login", readonly: true }, { name: "email", readonly: false }]);
  expect(field.nodes[0]).toMatchObject({ children: [{ field: { readonly: true } }, { field: { readonly: true } }, { field: { readonly: false } }] });
});

/// (partial: list arch create, edit, delete attribute parsing)
test("list view metadata admits create, edit and delete attributes", () => {
  const d = fixture();
  const list = {
    identity: { kind: "stored", id: 2 },
    type: "list",
    schema_major: 1,
    renderer: "web.list",
    root: 0,
    nodes: [
      { kind: "list", children: [1], create: false, edit: false, delete: true },
      d.views[0]!.nodes[1]!,
    ],
  };
  Object.assign(d, { views: [list] });
  const view = describeViews(d, "test.note", "r1").views.get("list")!;
  expect(view.canCreate).toBe(false);
  expect(view.canEdit).toBe(false);
  expect(view.canDelete).toBe(true);
});

/// (partial: form arch create, edit, delete attribute parsing)
test("form view metadata admits create, edit and delete attributes", () => {
  const d = fixture();
  Object.assign(d.views[0]!.nodes[0]!, { create: false, edit: true, delete: false });
  const view = describeViews(d, "test.note", "r1").views.get("form")!;
  expect(view.canCreate).toBe(false);
  expect(view.canEdit).toBe(true);
  expect(view.canDelete).toBe(false);
});

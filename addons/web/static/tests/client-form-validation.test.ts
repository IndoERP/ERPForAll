/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import "../src/client/fields.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { describeViews } from "../src/client/metadata.js";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { ClientError, RpcClient } from "../src/client/transport.js";
import { registerFieldWidget } from "../src/client/registry.js";

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });

const wire = (label: string, required: boolean) => ({ type: "char", label, required, readonly: false, relation: null, relation_field: null, selection: null });
const node = (name: string, label: string) => ({ kind: "field", force_save: false, field: { model: "test.note", name }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label });
/** Title on the sheet, a second required field on the second notebook page. */
function formWithTabs() {
  const rpc = new RpcClient(async () => new Response("{}"), () => {});
  const description = describeViews({ wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note",
    fields: { "test.note": { name: wire("Name", true), ref: wire("Reference", true), memo: wire("Memo", false) } }, notices: [],
    views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [
      { kind: "form", record_mode: "record", children: [1] }, { kind: "sheet", children: [2, 3] },
      { kind: "group", label: "", columns: 2, colspan: 2, children: [4] }, { kind: "notebook", children: [5, 6] },
      node("name", "Name"),
      { kind: "page", name: "one", label: "General", children: [7] }, { kind: "page", name: "two", label: "Extra", children: [8] },
      node("memo", "Memo"), node("ref", "Reference")] }] }, "test.note", "r1");
  const state = new RecordState(rpc, "test.note", description.views.get("form")!.fields, {});
  state.values = { name: "", ref: "", memo: "" };
  const form = document.createElement("form"); document.body.append(form);
  const notify = vi.fn();
  const surface = new RecordForm({ form, state, view: description.views.get("form")!, rpc, context: {}, run: () => {}, report: () => {}, notify, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render();
  return { form, surface, notify, state };
}
const selectedTab = (form: HTMLElement): string | null | undefined => form.querySelector('[role="tab"][aria-selected="true"]')?.textContent;

test("an empty required field is marked in place: class, aria-invalid, a message tied by aria-describedby, one count notice", () => {
  const { form, surface, notify } = formWithTabs();
  expect(surface.valid()).toBe(false);
  for (const name of ["name", "ref"]) {
    const input = form.querySelector<HTMLInputElement>(`input[name=${name}]`)!;
    const shell = input.closest<HTMLElement>(".field-shell")!;
    const note = shell.querySelector<HTMLElement>(":scope > .field-error")!;
    expect(shell.classList.contains("field-invalid")).toBe(true);
    expect(input.getAttribute("aria-invalid")).toBe("true");
    expect(note.hidden).toBe(false); expect(note.textContent).not.toBe("");
    expect((input.getAttribute("aria-describedby") ?? "").split(" ")).toContain(note.id);
  }
  expect(form.querySelector("input[name=memo]")?.hasAttribute("aria-invalid")).toBe(false);
  expect(notify).toHaveBeenCalledExactlyOnceWith("2 fields need a value");
  expect(document.activeElement).toBe(form.querySelector("input[name=name]"));
  surface.dispose();
});

test("the first invalid field in document order gets focus, and its notebook tab is revealed", () => {
  const { form, surface, notify, state } = formWithTabs();
  state.values = { name: "Done", ref: "", memo: "" };
  surface.render();
  expect(selectedTab(form)).toBe("General");
  expect(surface.valid()).toBe(false);
  expect(selectedTab(form)).toBe("Extra");
  expect(document.activeElement).toBe(form.querySelector("input[name=ref]"));
  expect(notify).toHaveBeenCalledExactlyOnceWith("1 field needs a value");
  surface.dispose();
});

test("a refusal from the server that names a field is shown on that field, not raised as an error", () => {
  const { form, surface, notify } = formWithTabs();
  const refused = new ClientError("validation", "Missing required field 'ref': it cannot be cleared", 400);
  expect(surface.flagServerError(refused)).toBe(true);
  const shell = form.querySelector("input[name=ref]")!.closest(".field-shell")!;
  expect(shell.classList.contains("field-invalid")).toBe(true);
  expect(shell.querySelector(".field-error")?.textContent).toBe("This field needs a value.");
  expect(selectedTab(form)).toBe("Extra");
  expect(document.activeElement).toBe(form.querySelector("input[name=ref]"));
  expect(notify).toHaveBeenCalledExactlyOnceWith("1 field needs a value");
  surface.dispose();
});

test("only a validation refusal that names a displayed field is handled; everything else still reaches the dialog", () => {
  const { surface, notify } = formWithTabs();
  expect(surface.flagServerError(new ClientError("validation", "Missing required field 'nothing_here'", 400))).toBe(false);
  expect(surface.flagServerError(new ClientError("validation", "The operation could not be completed.", 400))).toBe(false);
  expect(surface.flagServerError(new ClientError("server", "Missing required field 'ref'", 500))).toBe(false);
  expect(surface.flagServerError(new Error("Missing required field 'ref'"))).toBe(false);
  expect(notify).not.toHaveBeenCalled();
  surface.dispose();
});

const email: Field & { widget: string } = { name: "email", label: "Email", kind: "char", readonly: false, required: false, relation: null, relationField: null, selection: null, widget: "email" };
test("Open Email is an icon link in the field's own row, named for assistive technology", () => {
  const handle = createFieldControl({ field: email, value: "a@b.test", rpc: new RpcClient(async () => new Response("{}"), () => {}), context: {}, change: () => {}, run: () => {} });
  document.body.append(handle.node);
  const link = handle.node.querySelector<HTMLAnchorElement>(":scope > a.field-link-action")!;
  expect(link.getAttribute("href")).toBe("mailto:a%40b.test");
  expect(link.getAttribute("aria-label")).toBe("Open Email"); expect(link.title).toBe("Open Email");
  expect(link.querySelector("svg")).not.toBeNull(); expect(link.textContent).toBe("");
  expect(link.parentElement).toBe(handle.node);
});

test("a bare addon control gets its message as the next sibling, tied to it by aria-describedby", () => {
  registerFieldWidget("bare_title", { kinds: ["char"], render: environment => {
    const input = document.createElement("input"); input.name = environment.field.name; input.required = environment.field.required;
    return { node: input, dispose: () => {} };
  } });
  const rpc = new RpcClient(async () => new Response("{}"), () => {});
  const description = describeViews({ wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", fields: { "test.note": { name: wire("Name", true) } }, notices: [],
    views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, { ...node("name", "Name"), widget: "bare_title" }] }] }, "test.note", "r1");
  const state = new RecordState(rpc, "test.note", description.views.get("form")!.fields, {}); state.values = { name: "" };
  const form = document.createElement("form"); document.body.append(form);
  const surface = new RecordForm({ form, state, view: description.views.get("form")!, rpc, context: {}, run: () => {}, report: () => {}, notify: () => {}, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render();
  expect(surface.valid()).toBe(false);
  const input = form.querySelector("input")!;
  const note = input.nextElementSibling as HTMLElement;
  expect(note.classList.contains("field-error")).toBe(true); expect(note.hidden).toBe(false);
  expect(input.getAttribute("aria-describedby")).toBe(note.id);
  expect(input.querySelector(".field-error")).toBeNull();
  surface.dispose();
});

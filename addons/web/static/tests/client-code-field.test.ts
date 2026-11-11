/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import ace from "ace-builds";
import { fieldWidgets } from "../src/client/registry.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { RpcClient, ClientError } from "../src/client/transport.js";
import type { Field, View } from "../src/client/metadata.js";
import "../src/client/fields.js";
import { decodeAst, parseBoundedJson } from "../../../../web/packages/expr/src/index.js";

const disposers: (() => void)[] = [];
afterEach(() => { disposers.splice(0).forEach(stop => stop()); document.body.replaceChildren(); vi.restoreAllMocks(); });
function field(extra: Partial<Field> = {}): Field {
  return { name: "source", kind: "text", label: "Source", required: false, readonly: false, selection: null, relation: null, relationField: null, codeMode: "xml", ...extra };
}
async function fixture(extra: Partial<Field> = {}, onchange?: () => Promise<Record<string, unknown>>, saved?: () => Record<string, unknown>[] | Promise<Record<string, unknown>[]>, otherCode = false, widget = "code") {
  const f = field(extra), calls: { method: string; args: Record<string, unknown> }[] = [];
  let stored = "<form/>";
  const rpc = new RpcClient(async (path, init) => {
    const method = String(path).split("/").at(-1)!;
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>; calls.push({ method, args });
    if (method === "onchange") return new Response(JSON.stringify(onchange ? await onchange() : { value: {} }));
    if (method === "web_save") stored = String((args["vals"] as Record<string, unknown>)["source"] ?? stored);
    return new Response(JSON.stringify(method === "web_save" && saved ? await saved() : [{ id: 1, source: stored, ...(otherCode ? { other: "other original" } : {}) }]));
  }, () => {});
  const fields = otherCode ? [f, field({ name: "other", label: "Other" })] : [f];
  const state = new RecordState(rpc, "test.source", fields, {}); await state.load(1); calls.length = 0;
  const form = document.createElement("form"); document.body.append(form);
  const view: View = { type: "form", mode: "record", identity: { kind: "stored", id: 1 }, fields, nodes: [{ kind: "root", children: fields.map(field => ({ kind: "field", field, label: field.label, widget })) }] } as View;
  const report = vi.fn(), stage = vi.fn((name: string, value: unknown) => state.change(name, value));
  const surface = new RecordForm({ form, state, view, rpc, context: {}, report, run: job => { void job().catch(report); },
    stage, commit: async (name: string) => { await state.onchange(name); surface.render(); },
    openRecord: async () => {}, change: (name, value) => state.change(name, value), button: () => {} });
  disposers.push(() => surface.dispose()); surface.render();
  const engine = (name = "source") => ace.edit(form.querySelector<HTMLTextAreaElement>(`textarea[name="${name}"]`)!.closest(".code-field")!.querySelector<HTMLElement>(".ace_editor")!);
  return { form, surface, state, calls, report, engine, stage };
}
async function insert(engine: ReturnType<typeof ace.edit>, text: string) { engine.navigateFileEnd(); engine.insert(text); await Promise.resolve(); }

/// (partial: real-engine draft/echo/history and deferred commit; Chromium separate)
test("code stages immediately, retains its handle on render and commits only when requested", async () => {
  const f = await fixture(), engine = f.engine(), node = f.form.querySelector(".code-editor");
  await insert(engine, "\n<!--draft-->");
  expect(f.state.dirty).toBe(true); expect(f.calls).toEqual([]);
  expect(f.state.values["source"]).toBe("<form/>\n<!--draft-->");
  const cursor = engine.getCursorPosition();
  f.surface.render(); expect(f.form.querySelector(".code-editor")).toBe(node); expect(f.engine()).toBe(engine);
  expect(engine.getCursorPosition()).toEqual(cursor);
  expect(engine.getValue()).toBe(f.state.values["source"]);
  await f.surface.commit();
  expect(f.calls.map(call => call.method)).toEqual(["onchange"]);
  engine.execCommand("undo"); await Promise.resolve(); expect(engine.getValue()).toBe("<form/>"); expect(f.state.values["source"]).toBe("<form/>");
  engine.execCommand("redo"); await Promise.resolve(); expect(engine.getValue()).toBe("<form/>\n<!--draft-->"); expect(f.state.values["source"]).toBe(engine.getValue());
  expect(f.report).not.toHaveBeenCalled();
});

/// (partial: direct state save flush and discard baseline with real engine; server persistence separate)
test("save without blur commits once before write and discard drops pending source", async () => {
  const f = await fixture(); await insert(f.engine(), " saved");
  await f.state.save();
  expect(f.calls.map(call => call.method)).toEqual(["onchange", "web_save"]); expect(f.state.dirty).toBe(false);
  await insert(f.engine(), " discarded"); f.state.discard(); f.surface.render();
  expect(f.engine().getValue()).toBe("<form/> saved"); expect(f.state.dirty).toBe(false);
  await f.surface.commit(); expect(f.calls.map(call => call.method)).toEqual(["onchange", "web_save"]);
});

/// (partial: required full source and engine-level readonly/disabled validation)
test("required uses source document and disabled/readonly block native engine mutations", async () => {
  const f = await fixture({ required: true }); f.engine().setValue("", -1); await Promise.resolve();
  expect(f.surface.valid()).toBe(false); expect(f.form.querySelector(".code-editor-error")?.textContent).toBe("Source is required.");
  await insert(f.engine(), "valid"); expect(f.surface.valid()).toBe(true);
  f.surface.setDisabled(true); expect(f.engine().getReadOnly()).toBe(true);
  expect(f.form.querySelector<HTMLButtonElement>(".code-editor-history button")?.disabled).toBe(true);
  f.surface.setDisabled(false); expect(f.engine().getReadOnly()).toBe(false);
  const read = await fixture({ readonly: true }); expect(read.engine().getReadOnly()).toBe(true); expect(read.state.dirty).toBe(false);
});

/// (partial: adapter wrong type is precise and no coercion)
test("code adapter refuses nontext source values", () => {
  const widget = fieldWidgets.get("code");
  try {
    widget.render({ field: field(), value: 42, rpc: {} as RpcClient, context: {}, change: () => {}, run: () => {}, report: () => {} });
    throw new Error("Expected source refusal.");
  } catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message: "Code field Source must contain text." }); }
});

/// (partial: detached owner and authoritative reload reset local source)
test("reload creates a clean baseline and a disposed form cannot commit", async () => {
  const f = await fixture(); await insert(f.engine(), " unsaved");
  await f.state.load(1);
  expect(f.state.dirty).toBe(false); expect(f.engine().getValue()).toBe("<form/>");
  const writes = f.calls.filter(call => call.method === "web_save"); expect(writes).toEqual([]);
  f.surface.dispose();
  await expect(f.surface.commit()).rejects.toMatchObject({ kind: "validation", message: "The record form is disposed." });
  // Disposing removes the record's editor owner: no stale draft is flushed by a direct save.
  await f.state.save(); expect(f.calls.filter(call => call.method === "onchange")).toEqual([]);
});

/// (partial: stale onchange after discard cannot replace the restored baseline)
test("discard invalidates an in-flight code onchange before its values are applied", async () => {
  let release!: (value: Record<string, unknown>) => void;
  const reply = new Promise<Record<string, unknown>>(resolve => { release = resolve; });
  const f = await fixture({}, () => reply); await insert(f.engine(), " draft");
  const committing = f.surface.commit(); await vi.waitFor(() => expect(f.state.busy).toBe(true));
  f.state.discard(); release({ value: { source: "late source" } });
  await expect(committing).rejects.toMatchObject({ kind: "validation", message: "Onchange was superseded by a record reset." });
  expect(f.state.values["source"]).toBe("<form/>"); expect(f.engine().getValue()).toBe("<form/>"); expect(f.state.dirty).toBe(false);
});

/// (partial: malformed onchange source cannot replace an accepted draft)
test("a nontext onchange value refuses before replacing the accepted code draft", async () => {
  const f = await fixture({}, async () => ({ value: { source: 42 } })); await insert(f.engine(), " draft");
  await expect(f.surface.commit()).rejects.toMatchObject({ kind: "validation", message: "Code field Source must contain text." });
  expect(f.state.values["source"]).toBe("<form/> draft"); expect(f.engine().getValue()).toBe("<form/> draft");
});

/// (partial: every admitted mode reaches the actual field engine)
test("each served field mode selects its actual engine mode", async () => {
  for (const mode of ["xml", "python", "javascript", "json", "qweb", "scss", "bash", "text"] as const) {
    const f = await fixture({ codeMode: mode });
    expect(Reflect.get(f.engine().session.getMode(), "$id")).toBe(`ace/mode/${mode === "bash" ? "sh" : mode}`);
  }
});

/// (partial: malformed committed readback preserves draft and cannot replay a write)
test("malformed save readback preserves source under the uncertain-write lock until reload", async () => {
  const f = await fixture({}, undefined, () => [{ id: 1, source: 42 }]); await insert(f.engine(), " draft");
  await expect(f.state.save()).rejects.toMatchObject({ kind: "ambiguous", message: "The save response was invalid. Reload the record before another write." });
  expect(f.state.values["source"]).toBe("<form/> draft"); expect(f.engine().getValue()).toBe("<form/> draft");
  expect(f.state.writeUncertain).toBe(true); f.surface.setDisabled(false); expect(f.engine().getReadOnly()).toBe(true);
  await expect(f.state.save()).rejects.toMatchObject({ kind: "ambiguous", message: "Reload the record before another write; the previous change may already be saved." });
  expect(f.calls.filter(call => call.method === "web_save")).toHaveLength(1);
  await f.state.load(1); expect(f.state.writeUncertain).toBe(false); expect(f.engine().getReadOnly()).toBe(false);
  expect(f.engine().getValue()).toBe("<form/> draft");
});

/// (partial: an intentional discard cancels background blur commit feedback)
test("discard cancels an automatic blur commit without an error dialog callback", async () => {
  let release!: (value: Record<string, unknown>) => void;
  const reply = new Promise<Record<string, unknown>>(resolve => { release = resolve; });
  const f = await fixture({}, () => reply); const input = f.form.querySelector<HTMLTextAreaElement>("textarea")!;
  input.focus(); await insert(f.engine(), " draft"); input.blur();
  await vi.waitFor(() => expect(f.state.busy).toBe(true));
  f.state.discard(); release({ value: {} });
  await vi.waitFor(() => expect(f.state.busy).toBe(false)); await Promise.resolve();
  expect(f.state.values["source"]).toBe("<form/>"); expect(f.report).not.toHaveBeenCalled();
});

/// (partial: every direct RecordState save disables its attached code engine)
test("a direct save keeps code disabled until the write response completes", async () => {
  let release!: (value: Record<string, unknown>[]) => void;
  const response = new Promise<Record<string, unknown>[]>(resolve => { release = resolve; });
  const f = await fixture({}, undefined, () => response); await insert(f.engine(), " draft");
  const saving = f.state.save(); await vi.waitFor(() => expect(f.calls.filter(call => call.method === "web_save")).toHaveLength(1));
  expect(f.engine().getReadOnly()).toBe(true);
  release([{ id: 1, source: "<form/> draft" }]); await saving;
  expect(f.engine().getReadOnly()).toBe(false); expect(f.state.dirty).toBe(false);
});

/// (partial: disposal before save admission cannot dispatch a stale write)
test("disposing the editor while save admission awaits prevents its write", async () => {
  const f = await fixture(); const saving = f.state.save(); f.surface.dispose();
  await expect(saving).rejects.toMatchObject({ kind: "validation", message: "The record editor owner changed before saving." });
  expect(f.calls.filter(call => call.method === "web_save")).toEqual([]);
});

/// (partial: authoritative cross-field onchange replacement refreshes the commit baseline)
test("editing a server-replaced code field back to its original source still runs onchange", async () => {
  let replies = 0;
  const f = await fixture({}, async () => ({ value: replies++ === 0 ? { source: "server replacement" } : {} }));
  await f.state.onchange(""); f.surface.render();
  expect(f.engine().getValue()).toBe("server replacement");
  f.engine().setValue("<form/>", -1); await Promise.resolve();
  await f.state.save();
  expect(f.calls.map(call => call.method)).toEqual(["onchange", "onchange", "web_save"]);
});

/// (partial: code-to-code replacement and pending siblings retain field-specific onchange)
test("code onchange reconciles sibling baselines without dropping pending sibling commits", async () => {
  let replies = 0;
  const f = await fixture({}, async () => ({ value: replies++ === 0 ? { other: "server replacement" } : {} }), undefined, true);
  await insert(f.engine(), " draft"); await insert(f.engine("other"), " staged");
  await f.surface.commit();
  expect(f.calls.filter(call => call.method === "onchange").map(call => call.args["field_names"])).toEqual([["source"], ["other"]]);
  f.engine("other").setValue("other original", -1); await Promise.resolve(); await f.state.save();
  expect(f.calls.filter(call => call.method === "onchange").map(call => call.args["field_names"])).toEqual([["source"], ["other"], ["other"]]);
});

/// (partial: code-to-code authoritative replacement refreshes an untouched sibling baseline)
test("editing a code-replaced sibling back to its original source still commits it", async () => {
  let replies = 0;
  const f = await fixture({}, async () => ({ value: replies++ === 0 ? { other: "server replacement" } : {} }), undefined, true);
  await insert(f.engine(), " draft"); await f.surface.commit();
  f.engine("other").setValue("other original", -1); await Promise.resolve(); await f.state.save();
  expect(f.calls.filter(call => call.method === "onchange").map(call => call.args["field_names"])).toEqual([["source"], ["other"]]);
});

/// (partial: direct adapter use cannot silently choose a mode missing from admitted metadata)
test("code fields require an explicit admitted mode at the adapter boundary", () => {
  const missing = { ...field() }; delete missing.codeMode;
  try {
    const handle = fieldWidgets.get("code").render({ field: missing, value: "source", rpc: {} as RpcClient, context: {}, draft: { stage: () => {}, commit: () => {} }, change: () => {}, run: () => {}, report: () => {} });
    handle.dispose(); throw new Error("Expected explicit mode refusal.");
  } catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message: "Code editor mode is not supported." }); }
});

/// (partial: an ordinary form reset keeps addon/native widget ownership with its existing caller)
test("ordinary save does not rebuild its form from the optional draft reset hook", async () => {
  const f = await fixture({}, undefined, undefined, false, "text");
  const input = f.form.querySelector("textarea"); await f.state.save();
  expect(f.form.querySelector("textarea")).toBe(input);
});

/// (partial: a retained code editor follows dynamic readonly after the record changes, lead r1 delta MUST-2)
test("a code field that becomes editable through a dynamic readonly accepts typing", async () => {
  const lockedExpr = decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [{ kind: "name", name: "locked" }] })));
  const locked: Field = { name: "locked", kind: "boolean", label: "Locked", required: false, readonly: false, selection: null, relation: null, relationField: null };
  const source = field({ modifiers: { readonly: lockedExpr, required: null, invisible: null } });
  const rpc = new RpcClient(async () => new Response(JSON.stringify([{ id: 1, source: "<form/>", locked: true }])), () => {});
  const state = new RecordState(rpc, "test.source", [locked, source], {}); await state.load(1);
  const form = document.createElement("form"); document.body.append(form);
  const view = { type: "form", mode: "record", identity: { kind: "stored", id: 1 }, fields: [locked, source], nodes: [{ kind: "root", children: [{ kind: "field", field: source, label: source.label, widget: "code" }] }] } as View;
  const report = vi.fn();
  const surface = new RecordForm({ form, state, view, rpc, context: {}, report, run: job => { void job().catch(report); },
    stage: (name: string, value: unknown) => state.change(name, value), commit: async () => {},
    openRecord: async () => {}, change: (name, value) => state.change(name, value), button: () => {} });
  disposers.push(() => surface.dispose()); surface.render();
  const engine = () => ace.edit(form.querySelector<HTMLTextAreaElement>('textarea[name="source"]')!.closest(".code-field")!.querySelector<HTMLElement>(".ace_editor")!);
  expect(engine().getReadOnly()).toBe(true);
  state.change("locked", false); surface.render();
  expect(engine().getReadOnly()).toBe(false);
  await insert(engine(), " edited");
  expect(state.values["source"]).toBe("<form/> edited"); expect(report).not.toHaveBeenCalled();
});

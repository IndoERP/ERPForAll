/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { describeViews } from "../src/client/metadata.js";
import { ClientError } from "../src/client/transport.js";
import "../src/client/fields.js";
function wire(options: unknown) {
  return { notices: [], wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.source", fields: { "test.source": {
    source: { type: "text", label: "Source", readonly: false, required: false, selection: null, relation: null, relation_field: null },
  } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", renderer: "web.form", schema_major: 1, root: 0, nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "field", field: { model: "test.source", name: "source" }, widget: "code", label: "Source", force_save: false, modifiers: { readonly: null, required: null, invisible: null }, options },
  ] }] };
}

/// (partial: consumer retains every explicit served mode)
test("code field metadata retains closed mode configuration per occurrence", () => {
  for (const mode of ["xml", "python", "javascript", "json", "qweb", "scss", "bash", "text"]) {
    const view = describeViews(wire({ mode }), "test.source", "r1").views.get("form")!;
    expect(view.fields[0]?.codeMode).toBe(mode);
  }
});

/// (partial: invalid served modes refuse with kind and full message)
test("consumer never defaults an unknown or missing code mode", () => {
  for (const mode of ["XML", "unknown", "", null, 7, undefined]) {
    try { describeViews(wire(mode === undefined ? {} : { mode }), "test.source", "r1"); throw new Error("Expected invalid mode."); }
    catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject(mode === undefined
      ? { kind: "server", message: "Missing code field options member: mode." }
      : { kind: "validation", message: "Code editor mode is not supported." }); }
  }
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { RpcClient } from "../src/client/transport.js";

const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
const field = (widget: string, readonly = false, options: Record<string, unknown> = {}): Field & { widget: string; options: Record<string, unknown> } => ({
  name: "name", label: "Name", kind: "char", readonly, required: false, relation: null, relationField: null, selection: null, widget, options,
});
function control(f: Field, value: unknown = "example", change = vi.fn()) {
  const handle = createFieldControl({ field: f, value, rpc, context: {}, change, run: job => { void job(); } });
  document.body.append(handle.node); return { ...handle, change };
}
afterEach(() => document.body.replaceChildren());

/// (partial: password edit/reveal is value preserving and resets on disposal)
test("password input masks initially and reveal does not change the value", () => {
  const handle = control(field("password", false, { placeholder: "Secret", autocomplete: "new-password" }));
  const input = handle.node.querySelector("input")!;
  expect(input.type).toBe("password"); expect(input.placeholder).toBe("Secret"); expect(input.autocomplete).toBe("new-password");
  const reveal = handle.node.querySelector<HTMLButtonElement>("button")!;
  expect(reveal.getAttribute("aria-pressed")).toBe("false"); reveal.click();
  expect(input.type).toBe("text"); expect(reveal.getAttribute("aria-pressed")).toBe("true"); expect(handle.change).not.toHaveBeenCalled();
  handle.dispose(); expect(input.type).toBe("password");
});

/// (partial: readonly URL uses safe navigation and rejects executable schemes)
test("URL presentation refuses executable schemes and uses safe links", () => {
  const handle = control(field("url", true), "https://example.test/path");
  expect(handle.node.querySelector("a")?.getAttribute("href")).toBe("https://example.test/path");
  expect(handle.node.querySelector("input")).toBeNull();
  handle.dispose();
});

/// (partial: typed URL values never break rendering; unsafe values render as text, lead r1 M1)
test("URL values a user can type render in read and edit mode without throwing", () => {
  const cases: [string, string | null][] = [
    ["localhost:3000", "http://localhost:3000"],
    ["www.example.com:8080/x", "http://www.example.com:8080/x"],
    ["ftp://files.example.com", "ftp://files.example.com"],
    ["javascript:alert(1)", null],
    ["https://ok.test/\u0007", null],
  ];
  for (const [typed, href] of cases) {
    const read = control(field("url", true), typed);
    expect(read.node.querySelector("a")?.getAttribute("href") ?? null).toBe(href);
    expect(read.node.textContent).toContain(typed);
    read.dispose();
    const edit = control(field("url"), typed);
    expect(edit.node.querySelector("input")?.value).toBe(typed);
    expect(edit.node.querySelector("a.field-link-action")?.getAttribute("href") ?? null).toBe(href);
    edit.dispose();
  }
});

/// (partial: text placeholder and readonly multiline presentation)
test("text retains multiline value and declared placeholder", () => {
  const f = { ...field("text", false, { placeholder: "Description" }), kind: "text" as const };
  const handle = control(f, "one\ntwo");
  expect(handle.node.querySelector("textarea")?.placeholder).toBe("Description");
  expect(handle.node.querySelector("textarea")?.value).toBe("one\ntwo"); handle.dispose();
  const read = control({ ...f, readonly: true }, "one\ntwo");
  expect(read.node.textContent).toContain("one\ntwo"); expect(read.node.querySelector("textarea")).toBeNull(); read.dispose();
});

/// (partial: favorite and icon variants enforce readonly on their inner controls)
test("named boolean buttons preserve false as a value and enforce readonly", () => {
  for (const widget of ["boolean_favorite", "boolean_icon"]) {
    const handle = control({ ...field(widget), kind: "boolean", required: true }, false);
    const button = handle.node.querySelector<HTMLButtonElement>("button")!;
    expect(button).not.toBeNull(); expect(button.getAttribute("aria-pressed")).toBe("false");
    button.click(); expect(handle.change).toHaveBeenCalledWith(true); handle.dispose();
    const read = control({ ...field(widget, true), kind: "boolean" }, true);
    expect(read.node.querySelector<HTMLButtonElement>("button")?.disabled).toBe(true); read.dispose();
  }
});

/// (partial: overflow choice retention while mounting badge choices)
test("badge choice overflow mounts the actual select without reparenting failure", () => {
  const handle = control({ ...field("badges_selection", false, { badge_limit: 1 }), kind: "selection", selection: [["draft", "Draft"], ["ready", "Ready"]] }, "draft");
  expect(handle.node.querySelectorAll("button")).toHaveLength(1);
  expect(handle.node.querySelector("select")?.getAttribute("aria-label")).toBe("More Name choices");
  expect(handle.node.querySelector("select")?.value).toBe("draft"); handle.dispose();
});

/// (partial: color-dot tag dependencies use safe per-record color data)
test("tag color dots preserve related data and refuse injected style syntax", () => {
  const f: Field = { ...field("many2many_tags_color_dot", true), kind: "many2many", relation: "fixture.tag", options: { color_field: { model: "fixture.tag", name: "color" } } };
  const handle = control(f, [{ id: 1, display_name: "Blue", color: "#12abef" }]);
  expect(handle.node.querySelector<HTMLElement>(".relation-tag-dot")?.dataset["color"]).toBe("#12abef"); handle.dispose();
  expect(() => control(f, [{ id: 1, display_name: "Unsafe", color: "red;display:none" }])).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid tag color." }));
});

/// (partial: unchanged formatted numeric input preserves its represented value)
test("unchanged tiny numeric values do not require reparsing a display longer than the input cap", () => {
  const change = vi.fn();
  const handle = createFieldControl({ field: { ...field("float"), kind: "float" }, value: 5e-324, numericLocale: { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] }, rpc, context: {}, change, run: job => { void job(); } });
  const input = handle.node.querySelector<HTMLInputElement>("input")!;
  input.dispatchEvent(new Event("change"));
  expect(input.validity.valid).toBe(true); expect(change).not.toHaveBeenCalled(); handle.dispose();
});

/// (partial: admitted text-kind passwords mask in editable and readonly modes)
test("text-kind password fields never expose a textarea", () => {
  for (const readonly of [false, true]) {
    const handle = control({ ...field("password", readonly), kind: "text" }, "secret");
    const input = handle.node.querySelector<HTMLInputElement>("input")!;
    expect(handle.node.querySelector("textarea")).toBeNull(); expect(input).not.toBeNull(); expect(input.type).toBe("password");
    handle.node.querySelector<HTMLButtonElement>("button")!.click(); expect(input.type).toBe("text");
    expect(handle.change).not.toHaveBeenCalled(); handle.dispose(); expect(input.type).toBe("password");
  }
});

/// (partial: admitted numeric presentation and native input-type options are consumed)
test("numeric trailing-zero and input type options affect the actual control", () => {
  const options = { field: { ...field("float", true, { digits: [10, 2], hide_trailing_zeros: true }), kind: "float" as const }, value: 1.5, rpc, context: {}, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, change: vi.fn(), run: (job: () => Promise<void>) => { void job(); } };
  const read = createFieldControl(options); expect(read.node.textContent).toContain("1,5"); expect(read.node.textContent).not.toContain("1,50"); read.dispose();
  const edit = createFieldControl({ ...options, field: { ...options.field, readonly: false, options: { type: "number" } } });
  expect(edit.node.querySelector<HTMLInputElement>("input")?.type).toBe("number");
  expect(edit.node.querySelector<HTMLInputElement>("input")?.value).toBe("1.5"); edit.dispose();
});

/// (partial: normal and debug tooltip text use only authorized occurrence metadata)
test("debug tooltip distinguishes metadata from normal help without reading field values", async () => {
  const { fieldTooltip } = await import("../src/client/field-shell.js");
  const f = field("char", false, { help: "Help <script>text</script>" });
  expect(fieldTooltip(f, "char", false)).toBe("Help <script>text</script>");
  expect(fieldTooltip(f, "char", true)).toBe("Help <script>text</script>\n\nName (name)\nType: char\nWidget: char\nReadonly: false\nRequired: false");
});

/// (partial: generic, view and js-class dispatch; card component contract, not served kanban)
test("widget context dispatch precedence is explicit and unknown identity refuses", async () => {
  const { registerFieldWidget, resolveFieldWidget } = await import("../src/client/registry.js");
  const generic = { kinds: ["char" as const], render: () => ({ node: document.createElement("span"), dispose: () => {} }) };
  const list = { ...generic }, special = { ...generic }, card = { ...generic };
  registerFieldWidget("fixture_dispatch", generic); registerFieldWidget("list.fixture_dispatch", list); registerFieldWidget("fixture.fixture_dispatch", special); registerFieldWidget("card.fixture_dispatch", card);
  expect(resolveFieldWidget("fixture_dispatch", "form")).toBe(generic);
  expect(resolveFieldWidget("fixture_dispatch", "list")).toBe(list);
  expect(resolveFieldWidget("fixture_dispatch", "list", "fixture")).toBe(special);
  expect(resolveFieldWidget("fixture_dispatch", "card")).toBe(card);
  expect(() => resolveFieldWidget("fixture_unknown", "card")).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unknown field widget: fixture_unknown." }));
});

/// (partial: unavailable avatar transport refuses both empty and populated fields)
test("avatar tags explicitly refuse unavailable image transport before interaction", () => {
  for (const value of [[], [{ id: 1, display_name: "Fixture" }]]) {
    expect(() => control({ ...field("many2many_tags_avatar"), kind: "many2many", relation: "fixture.tag" }, value)).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Avatar tag data requires a later release." }));
  }
});

/// (partial: unavailable array dependency metadata refuses whitelist_fname explicitly)
test("selection array-field whitelist declaration reports its completion boundary", async () => {
  const { readFieldOptions } = await import("../src/client/field-options.js");
  expect(() => readFieldOptions({ whitelist_fname: { model: "fixture.record", name: "allowed" } }, "fixture.record", new Map([["allowed", {}]]))).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Selection whitelist field data requires a later release." }));
});

/// (partial: malformed scalar record data is refused rather than coerced)
test("scalar controls reject wrong represented value types and retain empty text policy", () => {
  expect(() => control(field("char"), { private: "object" })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid text value for name." }));
  expect(() => control({ ...field("float"), kind: "float" }, "1.2")).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid numeric value for name." }));
  const empty = control(field("char"), ""); expect(empty.node.classList.contains("field-empty")).toBe(true); empty.dispose();
});

/// (partial: selection placeholder dependency has precedence and empty false is not literal text)
test("selection and text placeholder fields consume only declared text values", () => {
  for (const widget of ["selection", "filterable_selection", "badges_selection", "char"]) {
    const f: Field = { ...field(widget, false, { placeholder: "Fallback", placeholder_field: { model: "fixture.record", name: "hint" }, ...(widget === "badges_selection" ? { badge_limit: 1 } : {}) }), ...(widget === "char" ? {} : { kind: "selection", selection: [["a", "A"], ["b", "B"]] }) };
    for (const [hint, expected] of [["Dynamic hint", "Dynamic hint"], [false, ""]] as const) {
      const handle = createFieldControl({ field: f, value: false, values: { hint }, rpc, context: {}, change: () => {}, run: () => {} });
      const control = handle.node.querySelector("input,select")!;
      expect(control instanceof HTMLSelectElement ? control.options[0]!.textContent : (control as HTMLInputElement).placeholder).toBe(expected); handle.dispose();
    }
    expect(() => createFieldControl({ field: f, value: false, values: { hint: 42 }, rpc, context: {}, change: () => {}, run: () => {} })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid scalar placeholder field value." }));
  }
});

/// (partial: non-editor boolean placeholders fail visibly instead of being silently ignored)
test("boolean placeholders are an invalid widget option", () => {
  expect(() => control({ ...field("boolean", false, { placeholder: "Ignored" }), kind: "boolean" }, false)).toThrowError(expect.objectContaining({ kind: "validation", message: "Widget 'boolean' does not support placeholder options." }));
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import "../src/client/fields.js";
import { createFieldControl } from "../src/client/fields.js";
import { configureClientTemplates, resolveFieldWidget, type WidgetEnvironment } from "../src/client/registry.js";
import { MessageServices } from "../src/client/messages.js";
import { RpcClient } from "../src/client/transport.js";
import type { Field } from "../src/client/metadata.js";
import { HumanApp } from "../src/client/app.js";
import { templates } from "../../../../web/tests/fixtures/client-landmark-templates.js";

const numericLocale = { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] };
const editorLocale = { lang: "en_GB", tz: "Asia/Jakarta", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" };
const stops: (() => void)[] = [];
const field = (name: string, kind: Field["kind"], widget: string = kind): Field => ({ name, kind, widget, label: name === "name" ? "Name" : name, readonly: true, required: false, selection: null, relation: null, relationField: null });
beforeEach(() => configureClientTemplates(templates));
afterEach(() => { for (const stop of stops.splice(0).reverse()) stop(); document.body.replaceChildren(); vi.unstubAllGlobals(); history.replaceState(null, "", "/"); });
function render(target: Field, value: unknown, extra: Partial<WidgetEnvironment> = {}) {
  const report = vi.fn(), messages = new MessageServices(document.body, report), change = vi.fn(); stops.push(() => messages.dispose());
  const rpc = new RpcClient(async () => { throw new Error("Unexpected list widget RPC"); }, () => {});
  const handle = resolveFieldWidget(target.widget ?? target.kind, "list").render({ field: target, value, viewContext: "list", messages, numericLocale, editorLocale, rpc, context: {}, change, report, run: job => { void job().catch(report); }, ...extra });
  stops.push(handle.dispose); document.body.append(handle.node); return { ...handle, change, report };
}

/// (partial: compact actual list adapters retain scalar identities and formatting)
test("list scalar shells omit form labels while form controls retain their labels", () => {
  const listed = render({ ...field("name", "char"), required: true }, "Notes");
  expect(listed.node.querySelector("label,.field-required-marker,.field-help")).toBeNull(); expect(listed.node.textContent).toBe("Notes");
  const regular = createFieldControl({ field: { ...field("name", "char"), readonly: false }, value: "Notes", rpc: new RpcClient(fetch, () => {}), context: {}, change: vi.fn(), run: () => {} }); stops.push(regular.dispose);
  expect(regular.node.querySelector("label")?.textContent).toBe("Name");
  const numeric = render({ ...field("amount", "float"), options: { digits: [10, 2] } }, 1234.5);
  expect(numeric.node.textContent).toBe("1.234,50"); expect(numeric.node.querySelector("input,button,label")).toBeNull();
});

test("list boolean and selection adapters are compact read values rather than disabled editors", () => {
  const flag = render(field("active", "boolean"), false); expect(flag.node.querySelector("input,button,label")).toBeNull(); expect(flag.node.textContent).toBe("No");
  const selected = render({ ...field("stage", "selection", "badge"), selection: [["ready", "Ready"]] }, "ready");
  expect(selected.node.textContent).toBe("Ready"); expect(selected.node.querySelector('.field-badge')).not.toBeNull(); expect(selected.node.querySelector("label,select")).toBeNull();
});

test("list relational labels and tags retain compact read presentation", () => {
  const relation = render({ ...field("partner_id", "many2one"), relation: "fixture.partner" }, { id: 1, display_name: "Alice" });
  expect(relation.node.textContent).toBe("Alice"); expect(relation.node.querySelector("input,label")).toBeNull();
  const tags = render({ ...field("partners", "many2many", "many2many_tags"), relation: "fixture.partner" }, [{ id: 1, display_name: "Alice" }, { id: 2, display_name: "Bob" }]);
  expect(tags.node.querySelectorAll('.relation-chip')).toHaveLength(2); expect(tags.node.querySelector("input,label,button")).toBeNull();
});

/// (partial: list date/datetime/range read values; served metadata and visual proof separate)
test("list dates and ranges show formatted text without disabled inputs or picker actions", () => {
  const date = render(field("scheduled", "date"), "2026-11-04"); expect(date.node.textContent).toBe("04/11/2026"); expect(date.node.querySelector("input,button,label")).toBeNull();
  const datetime = render({ ...field("appointment", "datetime"), options: { show_seconds: false } }, "2026-11-04 10:15:47");
  expect(datetime.node.textContent).toBe("04/11/2026 17:15"); expect(datetime.node.querySelector("input,button,label")).toBeNull();
  const start = { ...field("start", "date", "daterange"), options: { end_date_field: { model: "fixture.note", name: "end" }, always_range: true } }, end = field("end", "date");
  const range = render(start, "2026-11-04", { fields: [start, end], values: { start: "2026-11-04", end: "2026-11-08" } });
  expect(range.node.textContent).toBe("04/11/2026 – 08/11/2026"); expect(range.node.querySelector("input,button,label")).toBeNull(); expect(range.change).not.toHaveBeenCalled();
});

test("compact list read states retain named boolean glyphs and missing range endpoint position", () => {
  const favorite = render(field("favorite", "boolean", "boolean_favorite"), false);
  expect(favorite.node.textContent).toBe("☆"); expect(favorite.node.querySelector('[role="img"]')?.getAttribute('aria-label')).toBe("No");
  const icon = render({ ...field("flag", "boolean", "boolean_icon"), options: { icon: "✓" } }, true);
  expect(icon.node.textContent).toBe("✓"); expect(icon.node.querySelector('input,button')).toBeNull();
  const start = { ...field("start", "date", "daterange"), options: { end_date_field: { model: "fixture.note", name: "end" } } }, end = field("end", "date");
  const range = render(start, false, { fields: [start, end], values: { start: false, end: "2026-11-08" } });
  expect(range.node.textContent).toBe("— – 08/11/2026");
});

test("compact list locale errors remain read text with visible feedback", () => {
  const invalid = render(field("amount", "float"), 12.5, { numericLocale: { ...numericLocale, grouping: [0] } });
  expect(invalid.node.querySelector('input,label')).toBeNull(); expect(invalid.node.querySelector('.field-error')?.textContent).toBe("Invalid numeric locale.");
  expect(invalid.node.textContent).toContain("12.5");
});

/// (partial: asynchronous list currency presentation stays compact and reports unsafe amounts)
test("list monetary loading and resolved output have no form labels or input controls", async () => {
  let resolve!: (response: Response) => void;
  const rpc = new RpcClient(() => new Promise<Response>(done => { resolve = done; }), () => {});
  const amount = { ...field("amount", "monetary"), options: { currency_field: { model: "fixture.note", name: "currency_id" } } };
  const control = render(amount, 1234.5, { rpc, values: { currency_id: 1 } });
  expect(control.node.querySelector("input,label")).toBeNull(); expect(control.node.textContent).toBe("Loading currency presentation…");
  resolve(new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }));
  await vi.waitFor(() => expect(control.node.querySelector('.field-currency-symbol')).not.toBeNull());
  expect(control.node.textContent).toBe("1.234,50€"); expect(control.node.querySelector("input,button,label")).toBeNull();
});

test("list unsafe monetary amounts retain a visible refusal instead of an empty successful cell", async () => {
  const rpc = new RpcClient(async () => new Response('[{"id":1,"symbol":"€","position":"before","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }), () => {});
  const control = render({ ...field("amount", "monetary"), options: { currency_field: { model: "fixture.note", name: "currency_id" } } }, 0.125, { rpc, values: { currency_id: 1 } });
  await vi.waitFor(() => expect(control.node.textContent).toBe("This amount cannot round-trip safely. Lossless monetary transport requires a later release."));
  expect(control.node.querySelector("input,label")).toBeNull(); expect(control.change).not.toHaveBeenCalled();
});

test("list safe links and password reveal remain separate actions with no editable inputs", () => {
  const url = render(field("website", "char", "url"), "https://example.test"); expect(url.node.querySelector('a')?.href).toBe("https://example.test/"); expect(url.node.querySelector("label,input")).toBeNull();
  const secret = render(field("token", "char", "password"), "private value"); expect(secret.node.textContent).not.toContain("private value"); expect(secret.node.querySelector("input,label")).toBeNull();
  secret.node.querySelector<HTMLButtonElement>('.field-password-reveal')!.click(); expect(secret.node.textContent).toContain("private value"); expect(secret.change).not.toHaveBeenCalled();
  // An unsafe value renders as plain text in the row instead of breaking the list (lead r1 M1).
  const unsafe = render(field("website", "char", "url"), "javascript:alert(1)"); expect(unsafe.node.querySelector("a")).toBeNull(); expect(unsafe.node.textContent).toContain("javascript:alert(1)");
});

test("numeric Enter and Tab commit once while unchanged text retains its change notification", () => {
  const numericChange = vi.fn(), textChange = vi.fn(), rpc = new RpcClient(fetch, () => {});
  const numeric = createFieldControl({ field: { ...field("amount", "float"), readonly: false }, value: 2, numericLocale, rpc, context: {}, change: numericChange, run: () => {} }); stops.push(numeric.dispose);
  const input = numeric.node.querySelector<HTMLInputElement>("input")!; input.value = "3,5"; input.dispatchEvent(new Event("input")); input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  expect(numericChange).toHaveBeenCalledExactlyOnceWith(3.5); input.dispatchEvent(new Event("change")); expect(numericChange).toHaveBeenCalledTimes(1);
  input.value = "4,5"; input.dispatchEvent(new Event("input")); input.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })); expect(numericChange).toHaveBeenLastCalledWith(4.5);
  const text = createFieldControl({ field: { ...field("name", "char"), readonly: false }, value: "Already changed", rpc, context: {}, change: textChange, run: () => {} }); stops.push(text.dispose);
  text.node.querySelector<HTMLInputElement>("input")!.dispatchEvent(new Event("change")); expect(textChange).toHaveBeenCalledExactlyOnceWith("Already changed");
});

const bootstrap = { wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true }, companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1], locale: { ...editorLocale, ...numericLocale }, csrf: { logout: "test-token-not-secret" } };
async function listApp(kind: "float" | "char", widget: string, value: unknown) {
  const descriptor = { type: kind, label: "Primary", required: false, readonly: false, selection: null, relation: null, relation_field: null };
  const node = { kind: "field", field: { model: "fixture.note", name: "primary" }, widget, options: {}, modifiers: { readonly: null, required: null, invisible: null }, force_save: false, label: "Primary" };
  const description = { wire_major: 1, registry_version: "r1", description_version: "d1", model: "fixture.note", notices: [], fields: { "fixture.note": { primary: descriptor } }, views: [
    { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, node] },
    { identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, node] },
  ] };
  const action = { wire_major: 1, id: 20, name: "Notes", model: "fixture.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    const path = String(input); calls.push(path);
    const result = path.endsWith('erp_load_web_menus') ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : path.endsWith('erp_load_web_action_views') ? { action, views: description }
      : path.endsWith('erp_load_web_action') ? action
      : path.endsWith('erp_describe') ? { wire_major: 1, registry_version: "r1", scope: { kind: "global" }, declarations: [] }
      : path.endsWith('web_search_read') ? { length: 1, records: [{ id: 7, primary: value }] }
      : path.endsWith('web_read') ? [{ id: 7, primary: value }]
      : path.endsWith('get_views') ? description : undefined;
    if (result === undefined) throw new Error(`Unexpected app RPC: ${path}`);
    return new Response(JSON.stringify(result), { headers: { "Content-Type": "application/json" } });
  }));
  history.replaceState(null, "", "/erp/action-20"); const root = document.createElement('div');
  for (const region of ["header", "navigation", "notice", "content"]) { const element = document.createElement('div'); element.setAttribute(`data-client-${region}`, ""); root.append(element); }
  document.body.append(root); const app = new HumanApp(root, bootstrap); stops.push(() => app.dispose()); await app.start(); return { root, calls };
}

test("actual list retains root record Open action when first column is numeric", async () => {
  const { root, calls } = await listApp("float", "float", 12.5), cell = root.querySelector<HTMLTableCellElement>('tbody tr[data-id="7"] td:nth-child(2)')!;
  expect(cell.textContent).toBe("12,5"); expect(cell.querySelector("label,input")).toBeNull();
  const open = cell.querySelector<HTMLButtonElement>('[data-record-open]'); expect(open).not.toBeNull(); open!.click();
  await vi.waitFor(() => expect(calls.some(path => path.endsWith('web_read'))).toBe(true));
  expect(new URL(location.href).searchParams.get('id')).toBe("7");
});

test("actual list keeps URL action and root record navigation outside one another", async () => {
  const { root } = await listApp("char", "url", "https://example.test");
  const cell = root.querySelector<HTMLTableCellElement>('tbody tr[data-id="7"] td:nth-child(2)')!;
  expect(cell.querySelector('a')?.href).toBe("https://example.test/"); expect(cell.querySelector('[data-record-open]')).not.toBeNull();
  expect(cell.querySelector('[data-record-open] a,[data-record-open] input,[data-record-open] button')).toBeNull();
  expect(cell.querySelector('a')?.contains(cell.querySelector('[data-record-open]'))).toBe(false);
});

/// (partial: clipped primary list text retains its full formatted hover value)
test("primary readonly text preserves the full displayed value as the cell title", async () => {
  const { root } = await listApp("char", "char", "Long record title " + "department operations ".repeat(30));
  const cell = root.querySelector<HTMLTableCellElement>('tbody tr[data-id="7"] td:nth-child(2)')!;
  const open = cell.querySelector<HTMLElement>('[data-record-open]')!;
  expect(cell.title).toBe(open.textContent); expect(cell.title).toContain("Long record title");
});

/// (partial: a code field in a list shows its source as read-only text, lead r1 delta MUST-1)
test("list code cells render the source as text without the draft editor", () => {
  const code = render(field("arch", "text", "code"), "<list>\n  <field name=\"name\"/>\n</list>");
  expect(code.node.textContent).toBe("<list>\n  <field name=\"name\"/>\n</list>");
  expect(code.node.querySelector("textarea,input,label")).toBeNull();
  expect(render(field("arch", "text", "code"), false).node.textContent).toBe("");
});

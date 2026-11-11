/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl, fieldControl } from "../src/client/fields.js";
import { RpcClient } from "../src/client/transport.js";
import { PopoverService } from "../src/client/popover.js";
import { DialogService } from "../src/client/dialog-service.js";
import type { EditorEnvironment } from "../src/client/editor-types.js";
import type { Field } from "../src/client/metadata.js";

let relationServices: EditorEnvironment | undefined;
function relationEnvironment(): EditorEnvironment {
  if (!relationServices) { const report = (error: unknown): void => { throw error; }; relationServices = { messages: { popovers: new PopoverService(document.body), dialogs: new DialogService(report) }, report }; }
  return relationServices;
}
afterEach(() => { relationServices?.messages.popovers.dispose(); relationServices?.messages.dialogs.dispose(); relationServices = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); });


test("required Boolean permits an explicit false value through the real control", () => {
  const changed = vi.fn();
  const node = fieldControl({ field: { name: "active", kind: "boolean", label: "Active", readonly: false, required: true, relation: null, relationField: null, selection: null }, value: true, rpc: new RpcClient(fetch, () => {}), context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(node); const input = node.querySelector("input")!;
  input.checked = false; input.dispatchEvent(new Event("change"));
  expect(input.validity.valid).toBe(true); expect(changed).toHaveBeenCalledWith(false); node.remove();
});

test("boolean field renders toggle switch aligned with its label", () => {
  const field: Field = { name: "active", kind: "boolean", label: "Active", readonly: false, required: false, relation: null, relationField: null, selection: null };
  const node = fieldControl({ field: field, value: true, rpc: new RpcClient(fetch, () => {}), context: {}, change: vi.fn(), run: job => { void job(); }, editor: relationEnvironment() });
  expect(node.classList.contains("field-label-boolean")).toBe(true);
  const input = node.querySelector<HTMLInputElement>("input.boolean-toggle");
  expect(input).not.toBeNull();
  expect(input?.checked).toBe(true);
});

test("selection field renders as styled select with options", () => {
  const changed = vi.fn();
  const field: Field = { name: "state", kind: "selection", label: "Status", readonly: false, required: true, relation: null, relationField: null, selection: [["draft", "Draft"], ["done", "Done"]] };
  const node = fieldControl({ field: field, value: "draft", rpc: new RpcClient(fetch, () => {}), context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  const select = node.querySelector<HTMLSelectElement>("select.styled-select");
  expect(select).not.toBeNull();
  expect(select?.value).toBe("draft");
  expect(node.classList.contains("field-required")).toBe(true);

  select!.value = "done";
  select!.dispatchEvent(new Event("change"));
  expect(changed).toHaveBeenCalledWith("done");
});

test("many2one control searches records on input and selects via click", async () => {
  const calls: { path: string; body: unknown }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push({ path: String(path), body: JSON.parse(String(init?.body)) });
    return new Response(JSON.stringify([{ id: 10, display_name: "Alice" }, { id: 20, display_name: "Bob" }]), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const changed = vi.fn();
  const field: Field = { name: "user_id", kind: "many2one", label: "User", readonly: false, required: false, relation: "res.users", relationField: null, selection: null };
  const node = fieldControl({ field: field, value: false, rpc: rpc, context: { tz: "UTC" }, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(node);

  const input = node.querySelector<HTMLInputElement>("input.many2one-input")!;
  expect(input).not.toBeNull();

  // Type to search
  input.value = "Al";
  input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(calls.length).toBe(1));
  expect(calls[0]!.path).toBe("/erp/json/2/res.users/web_name_search");
  expect(calls[0]!.body).toMatchObject({ name: "Al", limit: 8 });

  // Dropdown should be visible with items
  const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
  await vi.waitFor(() => expect(dropdown.querySelectorAll('[role="option"]').length).toBe(3));
  const items = dropdown.querySelectorAll<HTMLElement>("[role=option]:not([aria-disabled=true])");
  expect([...items].map(item => item.textContent)).toEqual(["Alice", "Bob", 'Create "Al"']);

  // Click first item to select
  items[0]!.click();
  expect(changed).toHaveBeenCalledWith({ id: 10, display_name: "Alice" });
  expect(input.value).toBe("Alice");
  expect(dropdown.style.display).toBe("none");

  // Clear button should now be visible and clickable
  const clearBtn = node.querySelector<HTMLButtonElement>(".many2one-clear-btn")!;
  expect(clearBtn.style.display).not.toBe("none");
  clearBtn.click();
  expect(changed).toHaveBeenCalledWith(false);
  expect(input.value).toBe("");

  node.remove();
});

test("relation suggestions belong to the shared overlay rather than the field label", async () => {
  const rpc = new RpcClient(async () => new Response(JSON.stringify([{ id: 3, display_name: "Portal record" }]), { headers: { "Content-Type": "application/json" } }), () => {});
  const changed = vi.fn();
  const control = createFieldControl({ field: { name: "partner_id", kind: "many2one", label: "Partner", readonly: false, required: false, relation: "res.partner", relationField: null, selection: null }, value: false, rpc: rpc, context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(control.node); control.node.querySelector<HTMLInputElement>("input")!.click();
  await vi.waitFor(() => expect(document.querySelector('[role="option"]')?.textContent).toBe("Portal record"));
  expect(control.node.querySelector('[role="listbox"]')).toBeNull();
  expect(document.querySelector('.message-popover [role="listbox"]')).not.toBeNull();
  document.querySelector<HTMLElement>('[role="option"]')!.click(); expect(changed).toHaveBeenCalledWith({ id: 3, display_name: "Portal record" });
  control.dispose(); control.node.remove();
});

test("many2one keyboard navigation: ArrowDown, ArrowUp, Enter to select, Escape to close", async () => {
  const rpc = new RpcClient(async () => {
    return new Response(JSON.stringify([{ id: 10, display_name: "First" }, { id: 20, display_name: "Second" }]), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const changed = vi.fn();
  const field: Field = { name: "partner_id", kind: "many2one", label: "Partner", readonly: false, required: false, relation: "res.partner", relationField: null, selection: null };
  const node = fieldControl({ field: field, value: false, rpc: rpc, context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(node);

  const input = node.querySelector<HTMLInputElement>("input.many2one-input")!;
  input.click();
  await vi.waitFor(() => expect(document.querySelectorAll("[role=option]:not([aria-disabled=true])").length).toBe(2));

  const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
  expect(dropdown.style.display).not.toBe("none");

  // ArrowDown to highlight first item
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" }));
  const items = dropdown.querySelectorAll<HTMLElement>("[role=option]:not([aria-disabled=true])");
  expect(items[0]!.classList.contains("active")).toBe(true);

  // ArrowDown to highlight second item
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" }));
  expect(items[1]!.classList.contains("active")).toBe(true);

  // Enter to select highlighted second item
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
  expect(changed).toHaveBeenCalledWith({ id: 20, display_name: "Second" });
  expect(dropdown.style.display).toBe("none");

  // Trigger search again and press Escape to close
  input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(dropdown.style.display).not.toBe("none"));
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  expect(dropdown.style.display).toBe("none");

  node.remove();
});

test("many2one readonly renders static text and no dead open button", () => {
  const field: Field = { name: "company_id", kind: "many2one", label: "Company", readonly: true, required: false, relation: "res.company", relationField: null, selection: null };
  const node = fieldControl({ field: field, value: { id: 1, display_name: "Arkana Digital" }, rpc: new RpcClient(fetch, () => {}), context: {}, change: vi.fn(), run: job => { void job(); }, editor: relationEnvironment() });
  expect(node.classList.contains("field-readonly")).toBe(true);
  expect(node.textContent).toContain("Arkana Digital");
  // The open control was removed: no generic form route exists for an arbitrary related model.
  expect(node.querySelector(".many2one-open-btn")).toBeNull();
});

const m2oField: Field = { name: "user_id", kind: "many2one", label: "User", readonly: false, required: false, relation: "res.users", relationField: null, selection: null };
const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });

function deferred(): { promise: Promise<Response>; resolve: (value: unknown) => void } {
  let resolve!: (value: unknown) => void;
  const promise = new Promise<Response>(done => { resolve = value => done(json(value)); });
  return { promise, resolve };
}

test("many2one disposes shared overlay listeners and input searches", async () => {
  const add = vi.spyOn(document, "addEventListener"), remove = vi.spyOn(document, "removeEventListener");
  const fetcher = vi.fn(async () => json([{ id: 1, display_name: "Alice" }]));
  const control = createFieldControl({ field: m2oField, value: false, rpc: new RpcClient(fetcher, () => {}), context: {}, change: vi.fn(), run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(control.node); const input = control.node.querySelector<HTMLInputElement>("input")!; input.click();
  await vi.waitFor(() => expect(document.querySelector('[role="option"]')).not.toBeNull());
  const listener = add.mock.calls.find(call => call[0] === "pointerdown"); expect(listener).toBeDefined();
  control.dispose();
  expect(remove.mock.calls.some(call => call[0] === "pointerdown" && call[1] === listener![1])).toBe(true);
  input.dispatchEvent(new Event("input")); expect(fetcher).toHaveBeenCalledOnce(); expect(document.querySelector(".message-popover")).toBeNull();
  add.mockRestore(); remove.mockRestore();
});

test("many2one picking an option discards a name_search response still in flight", async () => {
  const pending = deferred();
  let calls = 0;
  const rpc = new RpcClient(async () => ++calls === 1 ? json([{ id: 5, display_name: "Alice" }]) : pending.promise, () => {});
  const changed = vi.fn();
  const control = createFieldControl({ field: m2oField, value: false, rpc: rpc, context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(control.node);
  const input = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
  input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(document.querySelectorAll("[role=option]:not([aria-disabled=true])").length).toBe(1));
  const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
  input.value = "Al";
  input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(calls).toBe(2));
  dropdown.querySelector<HTMLElement>("[role=option]:not([aria-disabled=true])")!.click();
  expect(changed).toHaveBeenCalledWith({ id: 5, display_name: "Alice" });
  expect(dropdown.style.display).toBe("none");
  pending.resolve([{ id: 7, display_name: "Late" }]);
  await new Promise(done => setTimeout(done, 20));
  expect(dropdown.style.display).toBe("none");
  expect(dropdown.textContent).not.toContain("Late");
  control.dispose(); control.node.remove();
});

test("many2one Escape discards a name_search response still in flight", async () => {
  const pending = deferred();
  const rpc = new RpcClient(() => pending.promise, () => {});
  const control = createFieldControl({ field: m2oField, value: false, rpc: rpc, context: {}, change: vi.fn(), run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(control.node);
  const input = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
  input.dispatchEvent(new Event("input"));
  const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  pending.resolve([{ id: 7, display_name: "Late" }]);
  await new Promise(done => setTimeout(done, 20));
  expect(dropdown.style.display).toBe("none");
  control.dispose(); control.node.remove();
});

test("many2one clearing a selection discards a name_search response still in flight", async () => {
  const pending = deferred();
  const jobs: Promise<void>[] = [];
  const changed = vi.fn();
  const control = createFieldControl({ field: m2oField, value: { id: 1, display_name: "Alice" }, rpc: new RpcClient(() => pending.promise, () => {}), context: {}, change: changed, run: job => { jobs.push(job()); }, editor: relationEnvironment() });
  document.body.append(control.node);
  try {
    const input = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
    input.focus(); input.click();
    const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
    expect(jobs).toHaveLength(1);
    control.node.querySelector<HTMLButtonElement>(".many2one-clear-btn")!.click();
    expect(changed).toHaveBeenCalledTimes(1);
    expect(changed).toHaveBeenCalledWith(false);
    expect(input.value).toBe("");

    pending.resolve([{ id: 7, display_name: "Late" }]);
    await Promise.all(jobs);
    expect(dropdown.style.display).toBe("none");
    expect(dropdown.textContent).not.toContain("Late");
    expect(input.getAttribute("aria-expanded")).toBe("false");
    expect(input.value).toBe("");
    expect(changed).toHaveBeenCalledTimes(1);
    expect(changed).toHaveBeenCalledWith(false);
  } finally {
    control.dispose(); control.node.remove();
  }
});

test("many2one blur restores the current name when typed text was never selected", () => {
  const changed = vi.fn();
  const control = createFieldControl({ field: m2oField, value: { id: 1, display_name: "Alice" }, rpc: new RpcClient(fetch, () => {}), context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  document.body.append(control.node);
  const input = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
  input.value = "Bo";
  input.dispatchEvent(new Event("blur"));
  expect(input.value).toBe("Alice");
  expect(changed).not.toHaveBeenCalled();
  control.dispose(); control.node.remove();
});

test("readonly relations expose actual navigation only when the host supplies it", async () => {
  const open = vi.fn(async () => {}), jobs: Promise<void>[] = [];
  const control = createFieldControl({ field: {...m2oField, readonly:true}, value: {id:4,display_name:"Alice"}, rpc: new RpcClient(fetch,()=>{}), context: {}, change: vi.fn(), run: job=>{jobs.push(job());}, openRecord: open, editor: relationEnvironment() });
  control.node.querySelector<HTMLButtonElement>('[aria-label="Open User: Alice"]')!.click();
  await Promise.all(jobs);expect(open).toHaveBeenCalledWith("res.users",4);control.dispose();
});

test("blur cancels a relation search before its first dropdown opens", async () => {
  const pending=deferred(),jobs:Promise<void>[]=[];
  const control=createFieldControl({ field: m2oField, value: {id:1,display_name:"Alice"}, rpc: new RpcClient(async()=>pending.promise,()=>{}), context: {}, change: vi.fn(), run: job=>{jobs.push(job());}, editor: relationEnvironment() });
  document.body.append(control.node);const input=control.node.querySelector<HTMLInputElement>("input")!;
  input.value="Bo";input.dispatchEvent(new Event("input"));input.dispatchEvent(new Event("blur"));
  pending.resolve([{id:2,display_name:"Bob"}]);await Promise.all(jobs);
  expect(input.value).toBe("Alice");expect(input.getAttribute("aria-expanded")).toBe("false");expect(control.node.textContent).not.toContain("Bob");control.dispose();control.node.remove();
});

test("many2one opens its list on click, not on focus, and a pick does not reopen it", async () => {
  const jobs: Promise<void>[] = [];
  let searches = 0;
  const rpc = new RpcClient(async () => {
    searches++;
    return new Response(JSON.stringify([{ id: 10, display_name: "Ada" }, { id: 20, display_name: "Bob" }]), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const render = (value: unknown): ReturnType<typeof createFieldControl> => createFieldControl({ field: m2oField, value: value, rpc: rpc, context: {}, change: picked => {
      control.dispose(); control.node.remove();
      control = render(picked);
      document.body.append(control.node);
      // A form re-render restores focus to the field the user just changed.
      control.node.querySelector<HTMLInputElement>("input.many2one-input")!.focus();
    }, run: job => { jobs.push(job()); }, editor: relationEnvironment() });
  let control = render(false);
  document.body.append(control.node);
  try {
    const input = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
    input.focus();
    await Promise.all(jobs);
    expect(searches).toBe(0);
    expect(document.querySelector(".editor-autocomplete-list")).toBeNull();

    input.click();
    await Promise.all(jobs);
    expect(searches).toBe(1);
    const dropdown = document.querySelector<HTMLElement>(".editor-autocomplete-list")!;
    expect(dropdown.style.display).toBe("block");
    input.click();
    expect(dropdown.style.display).toBe("none");

    input.click();
    await Promise.all(jobs);
    document.querySelector<HTMLElement>("[role=option]:not([aria-disabled=true])")!.click();
    await Promise.all(jobs);
    const next = control.node.querySelector<HTMLInputElement>("input.many2one-input")!;
    expect(next.value).toBe("Ada");
    expect(document.activeElement).toBe(next);
    expect(searches).toBe(2);
    expect(document.querySelector(".editor-autocomplete-list")).toBeNull();
  } finally {
    control.dispose(); control.node.remove();
  }
});

/// (partial: supported field affordances and required semantics, not rendered appearance)
test("date and relational affordances use local SVG icons while required stays native", () => {
  const rpc = new RpcClient(fetch, () => {}), run = (job: () => Promise<void>): void => { void job(); };
  const popovers = new PopoverService(document.body);
  const date = createFieldControl({ field: { ...m2oField, name: "date", kind: "date", relation: null, label: "Order date", required: true }, value: "2026-11-01", rpc, context: {}, change: vi.fn(), run, editor: { locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, popovers, report: () => {} } });
  expect(date.node.querySelector('input[type="text"]')?.hasAttribute("required")).toBe(true);
  expect(date.node.querySelector('[data-icon="calendar"]')?.getAttribute("aria-hidden")).toBe("true");
  expect(date.node.querySelector('.field-required-marker')?.textContent).toBe("*");
  const relation = createFieldControl({ field: m2oField, value: { id: 3, display_name: "Fixture partner" }, rpc, context: {}, change: vi.fn(), run, openRecord: async () => {}, editor: relationEnvironment() });
  expect(relation.node.querySelector('.many2one-clear-btn [data-icon="x"]')).not.toBeNull();
  expect(relation.node.querySelector('.many2one-open-btn [data-icon="arrow-left"]')).not.toBeNull();
  expect(relation.node.querySelector('.many2one-clear-btn')?.getAttribute("aria-label")).toBe("Clear selection");
  relation.dispose();
  date.dispose(); popovers.dispose();
});

/// (partial: empty required relation is rejected by native form validation)
test("required many2one keeps native required validation after clearing its actual selection", () => {
  const changed = vi.fn();
  const control = createFieldControl({ field: { ...m2oField, required: true }, value: { id: 3, display_name: "Fixture partner" }, rpc: new RpcClient(fetch, () => {}), context: {}, change: changed, run: job => { void job(); }, editor: relationEnvironment() });
  const input = control.node.querySelector<HTMLInputElement>("input")!;
  expect(input.checkValidity()).toBe(true);
  control.node.querySelector<HTMLButtonElement>(".many2one-clear-btn")!.click();
  expect(changed).toHaveBeenCalledWith(false);
  expect(input.checkValidity()).toBe(false);
  control.dispose();
});

test.each(["many2one", "many2many"] as const)("touch %s suggestions survive modal focus transfer and restore the original input", async kind => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; this.querySelector<HTMLButtonElement>("button")?.focus(); } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  vi.stubGlobal("matchMedia", () => ({ matches: true })); vi.stubGlobal("innerWidth", 390);
  const pending = deferred(), changed = vi.fn(), jobs: Promise<void>[] = [];
  const control = createFieldControl({ field: { ...m2oField, kind }, value: false, rpc: new RpcClient(() => pending.promise, () => {}), context: {}, change: changed, run: job => { jobs.push(job()); }, editor: relationEnvironment() });
  document.body.append(control.node); const input = control.node.querySelector<HTMLInputElement>("input")!; input.focus(); input.click();
  pending.resolve([{ id: 7, display_name: "Touch result" }]); await Promise.all(jobs);
  await vi.waitFor(() => expect(document.querySelector('dialog.editor-sheet [role="option"]:not([aria-disabled="true"])')?.textContent).toBe("Touch result"));
  expect(input.closest("dialog.editor-sheet")).not.toBeNull();
  document.querySelector<HTMLElement>('dialog.editor-sheet [role="option"]:not([aria-disabled="true"])')!.click();
  expect(changed).toHaveBeenCalledWith(kind === "many2one" ? { id: 7, display_name: "Touch result" } : [{ id: 7, display_name: "Touch result" }]);
  expect(control.node.contains(input)).toBe(true); expect(document.querySelector("dialog.editor-sheet")).toBeNull(); control.dispose();
});

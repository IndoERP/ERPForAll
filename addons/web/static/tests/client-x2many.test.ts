/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import { describeViews, type Description, type Field, type Subview, type View } from "../src/client/metadata.js";
import { RecordState, lineCommands, specification } from "../src/client/record.js";
import { fieldWidgets } from "../src/client/registry.js";
import { RpcClient } from "../src/client/transport.js";
import { PopoverService } from "../src/client/popover.js";
import { DialogService } from "../src/client/dialog-service.js";
import type { EditorEnvironment } from "../src/client/editor-types.js";
import { attachSubviews } from "../src/client/x2many.js";

let relationServices: EditorEnvironment | undefined;
function relationEnvironment(): EditorEnvironment {
  if (!relationServices) { const report = (error: unknown): void => { throw error; }; relationServices = { messages: { popovers: new PopoverService(document.body), dialogs: new DialogService(report) }, report }; }
  return relationServices;
}
afterEach(() => { relationServices?.messages.popovers.dispose(); relationServices?.messages.dialogs.dispose(); relationServices = undefined; document.body.replaceChildren(); });


const field = (name: string, kind: Field["kind"], extra: Partial<Field> = {}): Field =>
  ({ name, kind, label: name, required: false, readonly: false, relation: null, relationField: null, selection: null, ...extra });
const title = field("title", "char", { label: "Title", required: true });
const year = field("publication_year", "integer", { label: "Year" });
const author = field("author_id", "many2one", { label: "Author", relation: "library.author" });
const formView: View = {
  type: "form", mode: "record", fields: [title, year, author],
  nodes: [{ kind: "root", children: [{ kind: "sheet", children: [
    { kind: "field", field: title, label: "Title", widget: "char" },
    { kind: "field", field: year, label: "Year", widget: "integer" },
    { kind: "field", field: author, label: "Author", widget: "many2one" }
  ] }] }]
};
const subview: Subview = { model: "library.book", list: [title, year], form: formView, fields: new Map([[title.name, title], [year.name, year], [author.name, author]]) };
const books = (): Field => field("book_ids", "one2many", { label: "Books", relation: "library.book", relationField: "author_id", subview });
const jobs: Promise<void>[] = [];
const reports: unknown[] = [];
const run = (job: () => Promise<void>): void => { jobs.push(job()); };
Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
afterEach(() => { document.body.replaceChildren(); jobs.length = 0; reports.length = 0; });

function rpcWith(answer: (path: string, body: Record<string, unknown>) => unknown, calls: { path: string; body: Record<string, unknown> }[] = []): RpcClient {
  return new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });
    return new Response(JSON.stringify(answer(String(path), body)), { headers: { "Content-Type": "application/json" } });
  }, () => {});
}
function render(value: unknown, change = vi.fn(), rpc = rpcWith(() => ({})), target = books(), signal?: AbortSignal): { node: HTMLElement; change: typeof change } {
  const handle = fieldWidgets.get("one2many").render({ field: target, value, change, rpc, context: {}, run, signal, report: error => { reports.push(error); } });
  document.body.append(handle.node);
  return { node: handle.node, change };
}

test("one2many lines become create, update and delete commands against the stored lines", () => {
  const stored = [{ id: 1, title: "Kept" }, { id: 2, title: "Edited" }, { id: 3, title: "Removed" }];
  const current = [stored[0], { id: 2, title: "Edited again", publication_year: 1990, __changed: ["title"] }, { title: "New", publication_year: 2005, __key: "line_1" }];
  expect(lineCommands(books(), current, stored)).toEqual([
    [1, 2, { title: "Edited again" }],
    [0, 0, { title: "New", publication_year: 2005 }],
    [2, 3, 0]
  ]);
  expect(lineCommands(books(), stored, stored)).toEqual([]);
});

test("a one2many reads its lines with the fields of its embedded list and line form", () => {
  expect(specification([books()])).toEqual({ book_ids: { fields: {
    display_name: {}, title: {}, publication_year: {}, author_id: { fields: { display_name: {} } }
  } } });
  // Without the line views only the names are read.
  expect(specification([field("book_ids", "one2many", { relation: "library.book", relationField: "author_id" })]))
    .toEqual({ book_ids: { fields: { display_name: {} } } });
});

test("the embedded list shows the list columns, removes a line and adds one through the line dialog", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = rpcWith(path => path.endsWith("default_get") ? { publication_year: 2026 } : {}, calls);
  const { node, change } = render([{ id: 1, title: "Dune", publication_year: 1965 }], vi.fn(), rpc);
  expect([...node.querySelectorAll("th")].map(th => th.textContent)).toEqual(["Title", "Year", "Actions"]);
  // Cells use the list widgets; without a numeric locale (this fixture passes none) the integer widget shows the plain
  // number; the record form passes its numeric locale to the one2many (RecordForm in app.ts).
  expect([...node.querySelectorAll("tbody td")].slice(0, 2).map(td => td.textContent)).toEqual(["Dune", "1965"]);

  node.querySelector<HTMLButtonElement>(".x2many-remove")!.click();
  expect(change).toHaveBeenLastCalledWith([]);

  node.querySelector<HTMLButtonElement>(".x2many-add")!.click();
  await Promise.all(jobs);
  expect(calls[0]).toMatchObject({ path: "/erp/json/2/library.book/default_get", body: { fields: ["title", "publication_year"] } });
  const dialog = document.querySelector<HTMLDialogElement>("dialog.x2many-dialog")!;
  expect(dialog.querySelector("h2")!.textContent).toBe("Create Books");
  // The inverse many2one is set by the server, never shown in the line form.
  expect(dialog.querySelector('[name="author_id"]')).toBeNull();
  const save = [...dialog.querySelectorAll("button")].find(b => b.textContent === "Save & Close")!;
  save.click();
  // A required field still empty keeps the dialog open and reports which one.
  expect(reports).toEqual([expect.objectContaining({ kind: "validation", message: "Title is required." })]);
  expect(change).toHaveBeenCalledTimes(1);
  expect(dialog.isConnected).toBe(true);
  const input = dialog.querySelector<HTMLInputElement>('input[name="title"]')!;
  input.value = "Children of Dune"; input.dispatchEvent(new Event("change"));
  [...document.querySelectorAll<HTMLButtonElement>("dialog.x2many-dialog button")].find(b => b.textContent === "Save & Close")!.click();
  expect(change).toHaveBeenLastCalledWith([
    { id: 1, title: "Dune", publication_year: 1965 },
    expect.objectContaining({ title: "Children of Dune", publication_year: 2026, __key: expect.any(String) })
  ]);
  expect(document.querySelector("dialog.x2many-dialog")).toBeNull();
});

test("editing a stored line records which fields changed; a read-only one2many opens lines read-only", async () => {
  const { node, change } = render([{ id: 4, title: "Foundation", publication_year: 1951 }]);
  node.querySelector<HTMLElement>("tbody tr")!.click();
  await Promise.all(jobs);
  const dialog = document.querySelector<HTMLDialogElement>("dialog.x2many-dialog")!;
  const yearInput = dialog.querySelector<HTMLInputElement>('input[name="publication_year"]')!;
  yearInput.value = "1952"; yearInput.dispatchEvent(new Event("change"));
  [...document.querySelectorAll<HTMLButtonElement>("dialog.x2many-dialog button")].find(b => b.textContent === "Save & Close")!.click();
  expect(change).toHaveBeenCalledWith([{ id: 4, title: "Foundation", publication_year: 1952, __changed: ["publication_year"] }]);

  const readonly = render([{ id: 4, title: "Foundation" }], vi.fn(), undefined, { ...books(), readonly: true });
  expect(readonly.node.querySelector(".x2many-add")).toBeNull();
  expect(readonly.node.querySelector(".x2many-remove")).toBeNull();
  readonly.node.querySelector<HTMLElement>("tbody tr")!.click();
  await Promise.all(jobs);
  const view = document.querySelector<HTMLDialogElement>("dialog.x2many-dialog")!;
  expect(view.querySelector('input[name="title"]')).toBeNull();
  expect(view.textContent).toContain("Foundation");
  expect([...view.querySelectorAll("button")].map(b => b.textContent)).toEqual(["Close"]);
});

test("a stored line saved through the form is written as one2many commands", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = rpcWith(path => path.endsWith("web_save")
    ? [{ id: 9, name: "Le Guin", book_ids: [{ id: 2, title: "Earthsea" }] }]
    : [{ id: 9, name: "Le Guin", book_ids: [{ id: 1, title: "Old" }, { id: 2, title: "Earthsea" }] }], calls);
  const state = new RecordState(rpc, "library.author", [field("name", "char"), books()], {});
  await state.load(9);
  state.change("book_ids", [{ id: 2, title: "Earthsea, revised", __changed: ["title"] }, { title: "Lavinia", __key: "line_9" }]);
  await state.save();
  expect(calls.at(-1)!.body["vals"]).toEqual({ book_ids: [[1, 2, { title: "Earthsea, revised" }], [0, 0, { title: "Lavinia" }], [2, 1, 0]] });
  expect(state.values["book_ids"]).toEqual([{ id: 2, title: "Earthsea" }]);
});

test("one2many metadata requires its inverse, and the comodel views hide it", async () => {
  const raw = (relationField: string | null) => ({ wire_major: 1, registry_version: "r1", description_version: "d1", model: "library.author",
    fields: { "library.author": { book_ids: { type: "one2many", label: "Books", required: false, readonly: false, selection: null, relation: "library.book", relation_field: relationField } } },
    views: [{ identity: { kind: "generated" }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] },
      { kind: "field", force_save: false, field: { model: "library.author", name: "book_ids" }, widget: "one2many", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Books" }] }], notices: [] });
  expect(() => describeViews(raw(null), "library.author", "r1")).toThrowError(expect.objectContaining({ kind: "server" }));
  const description = describeViews(raw("author_id"), "library.author", "r1");
  const view = description.views.get("form")!;
  const comodel: Description = { model: "library.book", fields: new Map(subview.fields), views: new Map([["list", { type: "list", mode: "record", nodes: [], fields: [title, author] }], ["form", formView]]) };
  await attachSubviews(description, view, async () => comodel);
  expect(view.fields[0]!.subview!.list.map(f => f.name)).toEqual(["title"]);
  expect(description.fields.get("book_ids")!.subview).toBeUndefined();
  expect(view.fields[0]!.subview).toBeDefined();

  const failing = describeViews(raw("author_id"), "library.author", "r1");
  await attachSubviews(failing, failing.views.get("form")!, async () => { throw new Error("Access denied to library.book."); });
  const { node } = render([], vi.fn(), undefined, failing.views.get("form")!.fields[0]!);
  expect(node.querySelector(".x2many-note")!.textContent).toBe("These lines cannot be shown: Access denied to library.book.");
  expect(node.querySelector(".x2many-add")).toBeNull();
});

test("a line form's own one2many gets the comodel list columns; a model already on the chain keeps the name list", async () => {
  // Owner report 2026-11-04: Author → Books → open a book → its Loans showed only "Name" with
  // "sample_library.loan,21", because only the opened form's one2many fields had their views attached.
  const loanBook = field("book_id", "many2one", { label: "Book", relation: "library.book" });
  const borrower = field("borrower_id", "many2one", { label: "Borrower", relation: "res.partner" });
  const due = field("due_date", "date", { label: "Due Date" });
  const loanForm: View = { type: "form", mode: "record", fields: [loanBook, borrower, due], nodes: [] };
  const loans = field("loan_ids", "one2many", { label: "Loans", relation: "library.loan", relationField: "book_id" });
  const bookForm: View = { type: "form", mode: "record", fields: [title, author, loans], nodes: [] };
  const children = field("child_ids", "one2many", { label: "Contacts", relation: "res.partner", relationField: "parent_id" });
  const partnerForm: View = { type: "form", mode: "record", fields: [field("name", "char"), children], nodes: [] };
  const describe = (model: string, list: Field[], form: View): Description =>
    ({ model, fields: new Map(form.fields.map(f => [f.name, f])), views: new Map([["list", { type: "list", mode: "record", nodes: [], fields: list }], ["form", form]]) });
  const comodels: Record<string, Description> = {
    "library.book": describe("library.book", [title, author], bookForm),
    "library.loan": describe("library.loan", [loanBook, borrower, due], loanForm),
    "res.partner": describe("res.partner", [field("name", "char")], partnerForm)
  };
  const loaded: string[] = [];
  const load = async (model: string): Promise<Description> => { loaded.push(model); return comodels[model]!; };

  const bookIds = field("book_ids", "one2many", { label: "Books", relation: "library.book", relationField: "author_id" });
  const authorForm: View = { type: "form", mode: "record", fields: [field("name", "char"), bookIds], nodes: [] };
  await attachSubviews({ model: "library.author", fields: new Map(), views: new Map([["form", authorForm]]) }, authorForm, load);
  const nested = bookIds.subview!.form!.fields.find(f => f.name === "loan_ids")!;
  // The inverse (book_id) is hidden in the book's loan list, as for the top level.
  expect(nested.subview!.list.map(f => f.name)).toEqual(["borrower_id", "due_date"]);
  expect(loaded).toEqual(["library.book", "library.loan"]);
  const spec = specification(authorForm.fields) as { book_ids: { fields: { loan_ids: { fields: Record<string, unknown> } } } };
  expect(Object.keys(spec.book_ids.fields.loan_ids.fields)).toEqual(expect.arrayContaining(["display_name", "borrower_id", "due_date"]));

  // A partner's contacts are partners: the nested child_ids is not expanded again, and the
  // specification is finite.
  const top = field("child_ids", "one2many", { label: "Contacts", relation: "res.partner", relationField: "parent_id" });
  const topForm: View = { type: "form", mode: "record", fields: [top], nodes: [] };
  await attachSubviews({ model: "res.partner", fields: new Map(), views: new Map([["form", topForm]]) }, topForm, load);
  expect(top.subview!.list.map(f => f.name)).toEqual(["name"]);
  expect(partnerForm.fields.find(f => f.name === "child_ids")!.subview).toBeUndefined();
  expect(() => JSON.stringify(specification(topForm.fields))).not.toThrow();
});

test("many2many tags add from the suggestions, quick-create a typed name and remove with Backspace", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = rpcWith(path => path.endsWith("name_create") ? [12, "Space Opera"] : [{ id: 3, display_name: "Fantasy" }, { id: 5, display_name: "Science Fiction" }], calls);
  const genres = field("genre_ids", "many2many", { label: "Genres", relation: "library.genre" });
  const changed = vi.fn();
  const control = createFieldControl({ field: genres, value: [{ id: 3, display_name: "Fantasy" }], rpc: rpc, context: {}, change: changed, run: run, editor: relationEnvironment() });
  document.body.append(control.node);
  expect([...control.node.querySelectorAll(".relation-chip-name")].map(n => n.textContent)).toEqual(["Fantasy"]);
  const input = control.node.querySelector<HTMLInputElement>("input.many2many-input")!;
  input.value = "sci"; input.dispatchEvent(new Event("input"));
  await Promise.all(jobs);
  // The tag already linked is not offered again; the typed text can be created.
  await vi.waitFor(() => expect([...document.querySelectorAll("[role=option]:not([aria-disabled=true])")].map(n => n.textContent)).toEqual(["Science Fiction", 'Create "sci"']));
  document.querySelector<HTMLElement>("[role=option]:not([aria-disabled=true])")!.click();
  expect(changed).toHaveBeenLastCalledWith([{ id: 3, display_name: "Fantasy" }, { id: 5, display_name: "Science Fiction" }]);

  input.value = "Space Opera"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect([...document.querySelectorAll("[role=option]")].at(-1)?.textContent).toBe('Create "Space Opera"'));
  await Promise.all(jobs);
  [...document.querySelectorAll<HTMLElement>("[role=option]:not([aria-disabled=true])")].at(-1)!.click();
  await Promise.all(jobs);
  expect(calls.at(-1)).toMatchObject({ path: "/erp/json/2/library.genre/name_create", body: { name: "Space Opera" } });
  expect(changed).toHaveBeenLastCalledWith([{ id: 3, display_name: "Fantasy" }, { id: 12, display_name: "Space Opera" }]);

  input.value = "";
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Backspace" }));
  expect(changed).toHaveBeenLastCalledWith([]);
  control.dispose();
});


test("disposing a record scope removes a line modal without flushing staged edits", async () => {
  const abort = new AbortController();
  const { node, change } = render([], vi.fn(), rpcWith(() => ({title:"Draft"})), books(), abort.signal);
  node.querySelector<HTMLButtonElement>(".x2many-add")!.click(); await Promise.all(jobs);
  const dialog = document.querySelector<HTMLDialogElement>(".x2many-dialog")!;
  expect(dialog).not.toBeNull();
  [...dialog.querySelectorAll("button")].find(b=>b.textContent==="Save & New")!.click();
  abort.abort();
  await Promise.resolve();
  expect(document.querySelector(".x2many-dialog")).toBeNull();
  expect(change).not.toHaveBeenCalled();
});

test("a disposed scope cannot open a line modal after delayed defaults", async () => {
  const abort = new AbortController(); let resolve: ((value: Response) => void) | undefined;
  const rpc = new RpcClient(() => new Promise<Response>(done => {resolve=done;}),()=>{});
  const { node } = render([], vi.fn(), rpc, books(), abort.signal);
  node.querySelector<HTMLButtonElement>(".x2many-add")!.click();
  abort.abort(); resolve!(new Response(JSON.stringify({title:"Late"}),{headers:{"Content-Type":"application/json"}}));
  await Promise.all(jobs);
  expect(document.querySelector(".x2many-dialog")).toBeNull();
});

test("Save & New then Discard preserves saved draft lines and drops only the current draft", async () => {
  const { node, change } = render([], vi.fn(), rpcWith(() => ({title:"Saved draft"})));
  node.querySelector<HTMLButtonElement>(".x2many-add")!.click(); await Promise.all(jobs);
  const first = document.querySelector<HTMLInputElement>('.x2many-dialog input[name="title"]')!;
  [...document.querySelectorAll<HTMLButtonElement>(".x2many-dialog button")].find(b=>b.textContent==="Save & New")!.click();
  await vi.waitFor(()=>expect(document.querySelector<HTMLInputElement>('.x2many-dialog input[name="title"]')).not.toBe(first));
  const next = document.querySelector<HTMLInputElement>('.x2many-dialog input[name="title"]')!;
  next.value="Discard this draft"; next.dispatchEvent(new Event("change"));
  [...document.querySelectorAll<HTMLButtonElement>(".x2many-dialog button")].find(b=>b.textContent==="Discard")!.click();
  expect(change).toHaveBeenCalledTimes(1);
  expect(change).toHaveBeenCalledWith([expect.objectContaining({title:"Saved draft",__key:expect.any(String)})]);
});

/// (partial: readonly parent line dialogs keep both paired range inputs readonly)
test("readonly line dialog propagates policy into every range endpoint", async () => {
  const { MessageServices } = await import("../src/client/messages.js");
  const start = field("start", "date", { widget: "daterange", options: { end_date_field: { model: "fixture.line", name: "end" }, always_range: true } });
  const end = field("end", "date");
  const lineForm: View = { type: "form", mode: "record", fields: [start, end], nodes: [{ kind: "root", children: [{ kind: "field", field: start, widget: "daterange", label: "Start" }] }] };
  const target = { ...books(), readonly: true, subview: { model: "fixture.line", list: [start], form: lineForm, fields: new Map([["start", start], ["end", end]]) } };
  const messages = new MessageServices(document.body, error => { reports.push(error); }), change = vi.fn();
  const handle = fieldWidgets.get("one2many").render({ field: target, value: [{ id: 1, start: "2026-11-04", end: "2026-11-05" }], change, rpc: rpcWith(() => ({})), context: {}, messages, editorLocale: { lang: "en_GB", tz: "UTC", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" }, run, report: error => { reports.push(error); } });
  document.body.append(handle.node); handle.node.querySelector<HTMLElement>(".x2many-row-open")!.click(); await Promise.all(jobs);
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!, inputs = dialog.querySelectorAll<HTMLInputElement>("input");
  expect(inputs).toHaveLength(2); for (const input of inputs) expect(input.disabled).toBe(true);
  expect(dialog.querySelector('[aria-haspopup="dialog"]')).toBeNull();
  [...dialog.querySelectorAll("button")].find(button => button.textContent === "Close")!.click();
  expect(change).not.toHaveBeenCalled(); expect(reports).toEqual([]); handle.dispose(); messages.dispose();
});

/// (partial: mounted line modifiers and required validation precede parent commands)
test("line dialog reevaluates readonly invisible and required modifiers before commit", async () => {
  const { decodeAst, parseBoundedJson } = await import("../../../../web/packages/expr/src/index.js");
  const expression = decodeAst(parseBoundedJson('{"schema":1,"nodes":[{"kind":"name","name":"locked"}]}'));
  const locked = field("locked", "boolean"), title = field("title", "char", { modifiers: { readonly: expression, required: null, invisible: null } }), secret = field("secret", "char", { modifiers: { readonly: null, required: null, invisible: expression } }), code = field("code", "char", { modifiers: { readonly: null, required: expression, invisible: null } });
  const fields = [locked, title, secret, code];
  const form: View = { type: "form", mode: "record", fields, nodes: [{ kind: "root", children: fields.map(field => ({ kind: "field", field, label: field.label, widget: field.kind })) }] };
  const target = { ...books(), subview: { model: "fixture.line", list: [title], form, fields: new Map(fields.map(field => [field.name, field])) } };
  const before = [{ id: 1, locked: false, title: "Keep", secret: "Private", code: false }], change = vi.fn();
  const handle = render(before, change, rpcWith(() => ({})), target);
  handle.node.querySelector<HTMLElement>(".x2many-row-open")!.click(); await Promise.all(jobs);
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!;
  const checkbox = dialog.querySelector<HTMLInputElement>('[name="locked"]')!; checkbox.checked = true; checkbox.dispatchEvent(new Event("change"));
  expect(dialog.querySelector('[name="title"]')).toBeNull(); expect(dialog.querySelector('[name="secret"]')).toBeNull();
  expect(dialog.querySelector<HTMLInputElement>('[name="code"]')!.required).toBe(true);
  const save = (): void => [...dialog.querySelectorAll("button")].find(button => button.textContent === "Save & Close")!.click();
  save(); expect(dialog.isConnected).toBe(true); expect(reports[0]).toMatchObject({ kind: "validation", message: "code is required." });
  const input = dialog.querySelector<HTMLInputElement>('[name="code"]')!; input.value = "Set"; input.dispatchEvent(new Event("change")); save();
  expect(change).toHaveBeenCalledOnce(); expect(lineCommands(target, change.mock.calls[0]![0], before)).toEqual([[1, 1, { locked: true, code: "Set" }]]);
});

/// (partial: asynchronous monetary line drafts survive unrelated line rerenders)
test("line dialog retains invalid monetary draft through cached currency remount", async () => {
  const { MessageServices } = await import("../src/client/messages.js");
  const amount = field("amount", "monetary", { widget: "monetary", options: { currency_field: { model: "fixture.line", name: "currency_id" } } }), currency = field("currency_id", "many2one", { relation: "res.currency", readonly: true, invisible: true }), name = field("name", "char");
  const form: View = { type: "form", mode: "record", fields: [amount, currency, name], nodes: [{ kind: "root", children: [amount, name].map(field => ({ kind: "field", field, label: field.label, widget: field.widget ?? field.kind })) }] };
  const target = { ...books(), subview: { model: "fixture.line", list: [name], form, fields: new Map([amount, currency, name].map(field => [field.name, field])) } };
  const messages = new MessageServices(document.body, error => { reports.push(error); }), change = vi.fn();
  const rpc = rpcWith(() => [{ id: 1, symbol: "€", position: "after", decimal_places: 2 }]);
  const handle = fieldWidgets.get("one2many").render({ field: target, value: [{ id: 1, amount: 2, currency_id: { id: 1 }, name: "Before" }], change, rpc, context: {}, messages, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, run, report: error => { reports.push(error); } });
  document.body.append(handle.node); handle.node.querySelector<HTMLElement>(".x2many-row-open")!.click(); await Promise.all(jobs); await new Promise(done => setTimeout(done, 0));
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!;
  const input = dialog.querySelector<HTMLInputElement>('[name="amount"]')!; input.value = "invalid"; input.dispatchEvent(new Event("input"));
  const other = dialog.querySelector<HTMLInputElement>('[name="name"]')!; other.value = "After"; other.dispatchEvent(new Event("change")); await new Promise(done => setTimeout(done, 0));
  const restored = dialog.querySelector<HTMLInputElement>('[name="amount"]')!; expect(restored.value).toBe("invalid"); expect(restored.validity.valid).toBe(false);
  [...dialog.querySelectorAll("button")].find(button => button.textContent === "Save & Close")!.click(); expect(change).not.toHaveBeenCalled();
  expect(reports[0]).toMatchObject({ kind: "validation", message: "Correct the invalid line fields before saving." });
  [...dialog.querySelectorAll("button")].find(button => button.textContent === "Discard")!.click(); handle.dispose(); messages.dispose();
});

/// (partial: new line default_get omissions are empty authorized placeholder dependencies)
/// (partial: mounted new selection line opens with an absent optional hint)
test("new line opens when default_get omits an optional text placeholder dependency", async () => {
  const { MessageServices } = await import("../src/client/messages.js");
  const hint = field("hint", "char", { readonly: true, invisible: true });
  const ref = { model: "fixture.line", name: "hint" };
  const title = field("title", "char", { options: { placeholder_field: ref } });
  const choice = field("choice", "selection", { selection: [["a", "A"]], options: { placeholder_field: ref } });
  const date = field("date", "date", { options: { placeholder_field: ref } });
  const contact = field("contact", "many2one", { relation: "fixture.contact", options: { placeholder_field: ref, no_create: true } });
  const hintB = field("hint_b", "char", { readonly: true, invisible: true });
  const titleB = { ...title, options: { placeholder_field: { model: "fixture.line", name: "hint_b" } } };
  const fields = [title, titleB, choice, date, contact, hint, hintB];
  const form: View = { type: "form", mode: "record", fields, nodes: [{ kind: "root", children: fields.filter(f => !f.invisible).map(field => ({ kind: "field", field, label: field.label, widget: field.kind })) }] };
  const target = { ...books(), subview: { model: "fixture.line", list: [title], form, fields: new Map(fields.map(field => [field.name, field])) } };
  const messages = new MessageServices(document.body, error => { reports.push(error); });
  const handle = fieldWidgets.get("one2many").render({ field: target, value: [], change: vi.fn(), rpc: rpcWith(() => ({})), context: {}, messages, editorLocale: { lang: "en_GB", tz: "UTC", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" }, run, report: error => { reports.push(error); } });
  document.body.append(handle.node); handle.node.querySelector<HTMLButtonElement>(".x2many-add")!.click(); await Promise.all(jobs);
  expect(reports).toEqual([]);
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!; expect(dialog).not.toBeNull();
  expect(dialog.querySelectorAll('[name="title"]')).toHaveLength(2);
  for (const input of dialog.querySelectorAll<HTMLInputElement>('[name="title"]')) expect(input.placeholder).toBe("");
  for (const name of ["title", "date", "contact"]) expect(dialog.querySelector<HTMLInputElement>(`[name="${name}"]`)!.placeholder).toBe("");
  expect(dialog.querySelector<HTMLSelectElement>('[name="choice"]')!.options[0]!.textContent).toBe("");
  [...dialog.querySelectorAll("button")].find(button => button.textContent === "Discard")!.click(); handle.dispose(); messages.dispose();
});

/// (partial: embedded x2many columns mask password values like the main list, lead r1 M5)
test("x2many password columns never show the stored secret", () => {
  const token = field("api_token", "char", { label: "Token", widget: "password" });
  const flagged = field("pin", "char", { label: "PIN", options: { password: true } });
  const secretView: Subview = { ...subview, list: [title, token, flagged], fields: new Map([[title.name, title], [token.name, token], [flagged.name, flagged]]) };
  const { node } = render([{ id: 1, title: "Kept", api_token: "s3cr3t-token", pin: "4321" }, { id: 2, title: "Empty", api_token: false, pin: "" }], vi.fn(), rpcWith(() => ({})), field("book_ids", "one2many", { label: "Books", relation: "library.book", relationField: "author_id", subview: secretView }));
  expect(node.textContent).not.toContain("s3cr3t-token");
  expect(node.textContent).not.toContain("4321");
  const rows = [...node.querySelectorAll("tr.x2many-row")].map(row => [...row.querySelectorAll("td")].slice(0, 3).map(cell => cell.textContent));
  expect(rows).toEqual([["Kept", "••••••••", "••••••••"], ["Empty", "", ""]]);
});

/// (partial: a quick-created tag with color_field renders without colour instead of throwing, lead r1 M6)
test("quick-created tags carry an empty colour and render again without error", async () => {
  const rpc = rpcWith(path => path.endsWith("name_create") ? [12, "Urgent"] : []);
  const tags = field("tag_ids", "many2many", { label: "Tags", relation: "library.tag", widget: "many2many_tags", options: { color_field: { model: "library.tag", name: "color" } } });
  const changed = vi.fn();
  const control = createFieldControl({ field: tags, value: [{ id: 3, display_name: "Blue", color: 4 }], rpc, context: {}, change: changed, run, editor: relationEnvironment() });
  document.body.append(control.node);
  const input = control.node.querySelector<HTMLInputElement>("input.many2many-input")!;
  input.value = "Urgent"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect([...document.querySelectorAll("[role=option]")].at(-1)?.textContent).toBe('Create "Urgent"'));
  await Promise.all(jobs);
  [...document.querySelectorAll<HTMLElement>("[role=option]:not([aria-disabled=true])")].at(-1)!.click();
  await Promise.all(jobs);
  expect(changed).toHaveBeenLastCalledWith([{ id: 3, display_name: "Blue", color: 4 }, { id: 12, display_name: "Urgent", color: false }]);
  control.dispose();
  const again = createFieldControl({ field: tags, value: changed.mock.lastCall![0], rpc, context: {}, change: vi.fn(), run, editor: relationEnvironment() });
  document.body.append(again.node);
  const chips = [...again.node.querySelectorAll<HTMLElement>(".relation-chip")];
  expect(chips.map(chip => [chip.textContent?.includes("Urgent") ? "Urgent" : "Blue", chip.dataset["tagColor"] ?? null])).toEqual([["Blue", "4"], ["Urgent", null]]);
  again.dispose();
});

/// (partial: search_threshold is the minimum typed length before a relation search, lead r1 M3)
test("relation search waits for search_threshold characters and keeps the normal result count", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = rpcWith(() => [{ id: 5, display_name: "Science Fiction" }], calls);
  const genres = field("genre_ids", "many2many", { label: "Genres", relation: "library.genre", widget: "many2many_tags", options: { search_threshold: 3, no_create: true } });
  const control = createFieldControl({ field: genres, value: [], rpc, context: {}, change: vi.fn(), run, editor: relationEnvironment() });
  document.body.append(control.node);
  const input = control.node.querySelector<HTMLInputElement>("input.many2many-input")!;
  input.value = "sc"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect([...document.querySelectorAll("[role=option],[role=status]")].map(n => n.textContent)).toContain("Start typing 1 more character…"));
  expect(calls.filter(call => call.path.endsWith("web_name_search"))).toHaveLength(0);
  input.value = "sci"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(calls.filter(call => call.path.endsWith("web_name_search"))).toHaveLength(1));
  await Promise.all(jobs);
  expect(calls.find(call => call.path.endsWith("web_name_search"))!.body["limit"]).toBe(8);
  control.dispose();
});

/// (partial: an existing line dialog is titled with the relation field label, never the line identity)
test("existing line dialogs are titled Open: <field label> even when the line has only a technical name", async () => {
  const handle = render([{ id: 18, display_name: "sample_library.loan,18", title: "Dune" }]);
  handle.node.querySelector<HTMLElement>(".x2many-row-open")!.click();
  await Promise.all(jobs);
  const dialog = document.querySelector<HTMLDialogElement>("dialog.x2many-dialog")!;
  expect(dialog.querySelector("h2")!.textContent).toBe("Open: Books");
  expect(dialog.querySelector("h2")!.textContent).not.toContain("sample_library.loan");
});

test("embedded list cells use the comodel's list widgets: a colour swatch instead of the hex code", async () => {
  const cover = field("cover_color", "char", { label: "Cover Color", widget: "color" });
  const available = field("available", "boolean", { label: "Available" });
  const view: Subview = { ...subview, list: [title, cover, available], fields: new Map([[title.name, title], [cover.name, cover], [available.name, available], [year.name, year], [author.name, author]]) };
  const { node } = render([{ id: 1, title: "Bumi Manusia", cover_color: "#562424", available: true }], vi.fn(), rpcWith(() => ({})),
    field("book_ids", "one2many", { label: "Books", relation: "library.book", relationField: "author_id", subview: view }));
  const cells = [...node.querySelectorAll<HTMLTableCellElement>("tr.x2many-row td")];
  expect(cells[1]!.textContent).toBe("");
  const swatch = cells[1]!.querySelector<HTMLElement>(".field-color-swatch")!;
  expect(swatch.getAttribute("aria-label")).toBe("Cover Color: #562424");
  expect(swatch.style.backgroundColor).not.toBe("");
  expect(cells[2]!.classList.contains("x2many-boolean")).toBe(true);
  expect([...node.querySelectorAll("th")].map(th => th.className)).toEqual(["", "", "x2many-boolean", "x2many-actions"]);
  // A click on a widget cell still opens the line form.
  swatch.click();
  await Promise.all(jobs);
  expect(document.querySelector("dialog.x2many-dialog h2")!.textContent).toBe("Open: Books");
  expect(reports).toEqual([]);
});

test("one2many nolabel option suppresses caption while preserving aria-label", () => {
  const withLabel = books();
  const withoutLabel = { ...books(), options: { nolabel: true } };
  const h1 = render([], vi.fn(), rpcWith(() => ({})), withLabel);
  expect(h1.node.querySelector(".x2many-caption")?.textContent).toBe("Books");
  expect(h1.node.getAttribute("aria-labelledby")).not.toBeNull();

  const h2 = render([], vi.fn(), rpcWith(() => ({})), withoutLabel);
  expect(h2.node.querySelector(".x2many-caption")).toBeNull();
  expect(h2.node.getAttribute("aria-label")).toBe("Books");
});

test("openLine evaluates field context against parent record values and passes to default_get", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = rpcWith(() => ({ title: "Defaulted Child" }), calls);
  const contextDoc = {
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "default_parent_id" } },
      { kind: "name", name: "id" },
      { kind: "dict", items: [[0, 1]] }
    ]
  };
  const contextField = {
    ...books(),
    options: {
      context: contextDoc as unknown as import("../src/client/field-options.js").FieldPresentationOptions["context"]
    }
  };
  const handle = fieldWidgets.get("one2many").render({
    field: contextField,
    value: [],
    values: { id: 42 },
    change: vi.fn(),
    rpc,
    context: { ambient_key: "ambient_val" },
    run,
    report: error => { reports.push(error); }
  });
  document.body.append(handle.node);
  const addBtn = handle.node.querySelector<HTMLButtonElement>("button.x2many-add")!;
  addBtn.click();
  await Promise.all(jobs);

  const defaultGetCall = calls.find(call => call.path.endsWith("default_get"));
  expect(defaultGetCall).toBeDefined();
  expect(defaultGetCall?.body["context"]).toMatchObject({
    ambient_key: "ambient_val",
    default_parent_id: 42
  });
  const dialog = document.querySelector<HTMLDialogElement>("dialog.x2many-dialog")!;
  expect(dialog.querySelector("h2")!.textContent).toBe("Create Books");
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The pivot's spreadsheet download. The two fixtures under tests/fixtures are the
// documents `pivotExportDocument` produces for the tables below, and they are also what the server's parser
// (addons/web/src/pivot_export.rs) accepts: crates/erp-cli/tests/human_pivot_export.rs posts these same files to the
// real binary. If the client's document changes, the first assertion here fails until the fixture is updated, and the
// fixture is the only thing the Rust side reads.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, expect, test, vi } from "vitest";
import { describeViews, type View } from "../src/client/metadata.js";
import { decodePivotFields, PivotModel, pivotExportDocument, readPivotView, type PivotExportDocument, type PivotLimits } from "../src/client/pivot.js";
import { mountPivot } from "../src/client/pivot-view.js";
import { ClientError, fileNameOf, RpcClient } from "../src/client/transport.js";
import { recorder, records, type Rec } from "./pivot-fixture.js";

const fixture = (name: string): PivotExportDocument => JSON.parse(readFileSync(resolve(process.cwd(), "..", "addons/web/static/tests/fixtures", name), "utf8")) as PivotExportDocument;

const column = (type: string, label: string, extra: Record<string, unknown> = {}) =>
  ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null, ...extra });
const modelFields = {
  priority: column("selection", "Priority", { selection: [["normal", "Normal"], ["high", "High"]] }),
  state: column("selection", "State", { selection: [["draft", "Draft"], ["done", "Done"]] }),
  size: column("integer", "Size"), weight: column("float", "Weight")
};
/** What `fields_get` answers (crates/erp-orm/src/describe.rs). */
const fieldsReply = {
  priority: { string: "Priority", type: "selection", groupable: true, selection: [["normal", "Normal"], ["high", "High"]] },
  state: { string: "State", type: "selection", groupable: true, selection: [["draft", "Draft"], ["done", "Done"]] },
  size: { string: "Size", type: "integer", groupable: true, aggregator: "sum" },
  weight: { string: "Weight", type: "float", groupable: true, aggregator: "sum" }
};
const ref = (name: string) => ({ model: "t.note", name });
const field = (name: string, role: string, label: string) => ({ kind: "pivot_field", field: ref(name), label, role });
const view = (fields: unknown[]): View => describeViews({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.note", fields: { "t.note": modelFields }, notices: [],
  views: [{ identity: { kind: "stored", id: 5 }, type: "pivot", schema_major: 1, renderer: "web.pivot", root: 0,
    nodes: [{ kind: "pivot", children: fields.map((_, index) => index + 1), disable_linking: false, display_quantity: false }, ...fields] }]
}, "t.note", "r1").views.get("pivot")!;
const LIMITS: PivotLimits = { maxGroups: 50, maxOpen: 20, maxCells: 400 };
const american = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const nestedFields = () => [field("priority", "row", "Priority"), field("state", "col", "State"), field("size", "measure", "Size"), field("weight", "measure", "Weight")];
const flatFields = () => [field("priority", "row", "Priority"), field("size", "measure", "Size")];
const configOf = (fields: unknown[]) => readPivotView(view(fields), { ...decodePivotFields(fieldsReply), items: [] });
async function loaded(fields: unknown[], source: readonly Rec[] = records) {
  const model = new PivotModel(configOf(fields), recorder(source).call, () => [], LIMITS);
  await model.load();
  return model;
}
afterEach(() => document.body.replaceChildren());

/// (partial: the document of a table with two header levels and two measures)
test("the nested table is posted as header rows with spans, indented rows and typed cells: exactly the nested fixture", async () => {
  expect(pivotExportDocument(await loaded(nestedFields()), { model: "t.note", title: "Notes" })).toEqual(fixture("pivot-export-nested.json"));
});

/// (partial: slice 1's flat table)
test("a table without a column grouping is posted with one header level over the measure row: exactly the flat fixture", async () => {
  expect(pivotExportDocument(await loaded(flatFields()), { model: "t.note", title: "Notes" })).toEqual(fixture("pivot-export-flat.json"));
});

/// (partial: the posted table is the table on screen)
test("the posted headers and cells are the ones the table shows, in the same places", async () => {
  const model = await loaded(nestedFields());
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { void job(); } });
  document.body.append(pivot.element);
  const posted = pivotExportDocument(model, { model: "t.note", title: "Notes" });
  const shown = [...pivot.element.querySelectorAll("thead tr")].map(row => [...row.querySelectorAll("th")].map(cell => [cell.textContent!.trim(), cell.colSpan, cell.rowSpan]));
  expect(posted.headers.map(row => row.map(cell => [cell.label, cell.colspan ?? 1, cell.rowspan ?? 1]))).toEqual(shown);
  const text = (cell: PivotExportDocument["rows"][number]["cells"][number]): string => cell.value === undefined ? "" : cell.kind === "float" ? cell.value.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 }) : cell.value.toLocaleString("en-US");
  const rows = [...pivot.element.querySelectorAll("tbody tr")];
  expect(posted.rows.map(row => [row.label, ...row.cells.map(text)])).toEqual(rows.map(row => [row.querySelector("th")!.textContent!.trim(), ...[...row.querySelectorAll("td")].map(cell => cell.textContent!)]));
  expect(posted.rows.map(row => row.cells.map(cell => cell.bold === true))).toEqual(rows.map(row => [...row.querySelectorAll("td")].map(cell => cell.classList.contains("pivot-total"))));
});

test("a table with no records, an unavailable table and a table with no measure are refused instead of posted", async () => {
  const empty = await loaded(nestedFields(), []);
  expect(() => pivotExportDocument(empty, { model: "t.note", title: "Notes" })).toThrow(ClientError);
  const model = await loaded(flatFields());
  const failing = new PivotModel(configOf(flatFields()), async () => { throw new ClientError("server", "no"); }, () => [], LIMITS);
  await expect(failing.load()).rejects.toThrow();
  expect(() => pivotExportDocument(failing, { model: "t.note", title: "Notes" })).toThrow("There is no table to download.");
  await model.toggleMeasure("size");
  expect(() => pivotExportDocument(model, { model: "t.note", title: "Notes" })).toThrow("There is no table to download.");
});

// ---- the toolbar button ----

const mountWith = async (send: (posted: PivotExportDocument) => Promise<void>, fields = flatFields(), source: readonly Rec[] = records) => {
  const model = await loaded(fields, source);
  const jobs: Promise<void>[] = [];
  const failures: unknown[] = [];
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { jobs.push(job().catch((error: unknown) => { failures.push(error); })); }, download: { model: "t.note", send } });
  document.body.append(pivot.element);
  return { model, pivot, failures, settle: async () => { await Promise.all(jobs.splice(0)); }, button: () => pivot.element.querySelector<HTMLButtonElement>(".pivot-download")! };
};

/// (partial: the Download button; flip and expand all come with their slices)
test("the Download button is named, inside the Main actions toolbar, posts the table once and is enabled again afterwards", async () => {
  const sent: PivotExportDocument[] = [];
  let finish: () => void = () => undefined;
  const { button, settle, pivot } = await mountWith(posted => new Promise(resolve => { sent.push(posted); finish = resolve; }));
  expect(button().textContent).toBe("Download");
  expect(button().getAttribute("aria-label")).toBe("Download xlsx");
  expect(button().closest("[role=toolbar]")!.getAttribute("aria-label")).toBe("Main actions");
  expect(button().disabled).toBe(false);
  button().click();
  await vi.waitFor(() => expect(sent).toHaveLength(1));
  expect(sent[0]).toEqual(fixture("pivot-export-flat.json"));
  expect(button().disabled).toBe(true); // a second press cannot start a second download
  expect(button().title).toBe("The download is being prepared.");
  finish(); await settle();
  expect(button().disabled).toBe(false);
  expect(sent).toHaveLength(1);
  pivot.dispose();
});

/// (partial: the Download button is disabled, with its reason, when there is nothing to export)
test("the Download button is disabled with its reason when no record matches, and when the table could not be read", async () => {
  const empty = await mountWith(async () => undefined, flatFields(), []);
  expect(empty.button().disabled).toBe(true);
  expect(empty.button().title).toBe("There is nothing to show: no record matches the search.");
  const broken = new PivotModel(configOf(flatFields()), async () => { throw new ClientError("server", "no"); }, () => [], LIMITS);
  await broken.load().catch(() => undefined);
  const unavailable = mountPivot({ model: broken, locale: american, title: "Notes", run: job => { void job(); }, download: { model: "t.note", send: async () => undefined } });
  const button = unavailable.element.querySelector<HTMLButtonElement>(".pivot-download")!;
  expect(button.disabled).toBe(true);
  expect(button.title).toBe("The table could not be loaded, so there is nothing to download.");
  unavailable.dispose(); empty.pivot.dispose();
});

test("a download the server refuses is reported by the job runner and leaves the button usable", async () => {
  const { button, settle, failures } = await mountWith(async () => { throw new ClientError("permission", "You are not allowed to read this model."); });
  button().click();
  await settle();
  expect(failures).toHaveLength(1);
  expect((failures[0] as ClientError).message).toBe("You are not allowed to read this model.");
  expect(button().disabled).toBe(false);
});

test("without a download host there is no Download button", async () => {
  const model = await loaded(flatFields());
  const pivot = mountPivot({ model, locale: american, title: "Notes", run: job => { void job(); } });
  expect(pivot.element.querySelector(".pivot-download")).toBeNull();
});

// ---- the request ----

const XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const client = (respond: (input: RequestInfo | URL, init?: RequestInit) => Response, expired: () => void = () => undefined) =>
  new RpcClient(async (input, init) => respond(input, init), expired);

/// (partial: the request the route accepts: POST, JSON, same origin credentials)
test("the document is posted as JSON to the download route and the workbook and its name are returned", async () => {
  let seen: { path: string; init?: RequestInit } | undefined;
  const rpc = client((input, init) => {
    seen = { path: String(input), ...(init ? { init } : {}) };
    return new Response("PK", { status: 200, headers: { "Content-Type": XLSX, "Content-Disposition": "attachment; filename=\"Notes_.xlsx\"; filename*=UTF-8''Notes%20%C3%A9.xlsx" } });
  });
  const posted = fixture("pivot-export-flat.json");
  const { blob, fileName } = await rpc.postFile("/erp/pivot/export", posted);
  expect(seen!.path).toBe("/erp/pivot/export");
  expect(seen!.init!.method).toBe("POST");
  expect(seen!.init!.credentials).toBe("same-origin");
  expect((seen!.init!.headers as Record<string, string>)["Content-Type"]).toBe("application/json");
  expect(JSON.parse(seen!.init!.body as string)).toEqual(posted);
  expect([blob.type, blob.size, fileName]).toEqual([XLSX, 2, "Notes é.xlsx"]);
});

test("a refusal is shown as the server worded it: a JSON error message, plain text, an expired session and a wrong answer", async () => {
  const refuse = (status: number, body: string, type = "text/plain") => client(() => new Response(body, { status, headers: { "Content-Type": type } })).postFile("/erp/pivot/export", {});
  await expect(refuse(403, JSON.stringify({ name: "AccessError", message: "No read access to t.note." }), "application/json")).rejects.toMatchObject({ kind: "permission", message: "No read access to t.note." });
  await expect(refuse(422, "too many cells: web.pivot_export_max_cells")).rejects.toMatchObject({ kind: "validation", message: "too many cells: web.pivot_export_max_cells" });
  await expect(refuse(413, "")).rejects.toMatchObject({ kind: "validation", status: 413 });
  await expect(refuse(500, "Internal server error")).rejects.toMatchObject({ kind: "server" });
  let expired = 0;
  await expect(client(() => new Response("", { status: 401 }), () => { expired++; }).postFile("/erp/pivot/export", {})).rejects.toMatchObject({ kind: "session" });
  expect(expired).toBe(1);
  await expect(client(() => new Response("<html></html>", { status: 200, headers: { "Content-Type": "text/html" } })).postFile("/erp/pivot/export", {})).rejects.toMatchObject({ kind: "server" });
  await expect(new RpcClient(async () => { throw new TypeError("offline"); }, () => undefined).postFile("/erp/pivot/export", {})).rejects.toMatchObject({ kind: "network" });
});

test("a failed download that is not a plain sentence or a JSON message shows a fixed sentence, never the raw body", async () => {
  const refuse = (status: number, body: string, type: string) => client(() => new Response(body, { status, headers: { "Content-Type": type } })).postFile("/erp/pivot/export", {});
  await expect(refuse(502, "<html><body>Bad gateway</body></html>", "text/html")).rejects.toMatchObject({ kind: "server", message: "The server is temporarily unavailable. Please try again shortly." });
  await expect(refuse(500, "Internal server error", "text/plain")).rejects.toMatchObject({ kind: "server", message: "The server is temporarily unavailable. Please try again shortly." });
  await expect(refuse(400, "<html>nope</html>", "text/html")).rejects.toMatchObject({ kind: "validation", message: "The download was refused. Please reload and try again." });
});

test("the download name is the exact UTF-8 name when sent, else the ASCII one, else a neutral name", () => {
  expect(fileNameOf("attachment; filename=\"a_.xlsx\"; filename*=UTF-8''a%C3%A9.xlsx")).toBe("aé.xlsx");
  expect(fileNameOf("attachment; filename=\"plain.xlsx\"")).toBe("plain.xlsx");
  expect(fileNameOf("attachment; filename*=UTF-8''%E0%A4%A")).toBe("download.xlsx");
  expect(fileNameOf(null)).toBe("download.xlsx");
});

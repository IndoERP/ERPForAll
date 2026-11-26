/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Kanban columns (M4.KANBAN-CLIENT-COLUMNS): the grouped read's admission, the fold state of the page address, the board
// (headers, folded strips, load more, a column's own failure, the keyboard) and the application path (default grouping,
// Group By, fold, unfold, load more, reload, the way back from a card, the bound on columns).
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { describeViews, type View } from "../src/client/metadata.js";
import { kanbanLayout } from "../src/client/kanban.js";
import { decodeKanbanGroups, kanbanOpeningInfo, renderKanbanBoard, type KanbanColumnState } from "../src/client/kanban-columns.js";
import { decodeKanbanFold, encodeKanbanFold, validKanbanFold, withKanbanColumn } from "../src/client/kanban-fold.js";
import { parseRoute, routeUrl } from "../src/client/route.js";
import type { GroupableField } from "../src/client/search/types.js";
import { auditComponent } from "../../../../web/testing/axe.js";

const column = (type: string, label: string, extra: Record<string, unknown> = {}) =>
  ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null, ...extra });
const modelFields = {
  name: column("char", "Name"),
  state: column("selection", "State", { selection: [["draft", "Draft"], ["done", "Done"]] }),
  day: column("date", "Day"),
  volume: column("integer","Volume"),
};
const ref = (name: string) => ({ model: "t.note", name });
const modifiers = { readonly: null, required: null, invisible: null };
const field = (name: string, widget: string, label: string) => ({ kind: "field", force_save: false, field: ref(name), widget, options: {}, modifiers, label });
type Root = { page_limit?: number; default_group_by?: unknown; can_open?: boolean; progress?: boolean; sum?: boolean; uncolored?: boolean; archive?: boolean; create?: boolean; edit?: boolean };
const kanbanNodes = (root: Root = {}) => [
  { kind: "kanban", children: root.progress ? [1, 5] : [1], ...(root.create === undefined ? {} : {create: root.create}), ...(root.edit === undefined ? {} : {edit: root.edit}), quick_create: true, records_draggable: true, groups_draggable: true, archivable: true, can_open: root.can_open ?? true,
    ...(root.page_limit ? { page_limit: root.page_limit } : {}), ...(root.default_group_by ? { default_group_by: root.default_group_by } : {}) },
  { kind: "kanban_card", children: [2, 3] },
  field("name", "char", "Name"),
  { kind: "kanban_section", region: "footer", children: [4] },
  field("state", "selection", "State"),
  ...(root.progress ? [{kind: "kanban_progressbar", field: ref("state"), colors: [{value: "draft", tone: "info"}, ...(root.uncolored ? [] : [{value: "done", tone: "success"}])], ...(root.sum ? {sum_field: ref("volume")} : {})}] : []),
];
const kanbanDescription = (root?: Root) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.note", fields: { "t.note": { ...modelFields, ...(root?.archive ? { active: column("boolean","Active") } : {}) } }, notices: [],
  views: [{ identity: { kind: "stored", id: 7 }, type: "kanban", schema_major: 1, renderer: "web.kanban", root: 0, nodes: kanbanNodes(root) }]
});
const view = (root?: Root): View => describeViews(kanbanDescription(root), "t.note", "r1").views.get("kanban")!;

// ---- the grouped read's answer ----

const stateField: GroupableField = { name: "state", label: "State", kind: "selection", selection: [["draft", "Draft"], ["done", "Done"]] };
const ownerField: GroupableField = { name: "owner_id", label: "Owner", kind: "many2one", selection: null };
const group = (value: unknown, count: number, records?: unknown[], spec = "state") =>
  ({ [spec]: value, __count: count, __extra_domain: [[spec.split(":")[0], "=", Array.isArray(value) ? value[0] : value]], ...(records ? { __records: records } : {}) });

/// SPEC-VIEW-043 (partial: the answer of the one grouped read: a column that carries its first cards is open, one without is folded)
/// SPEC-VIEW-044 (partial: the unset value's column is first and is folded when the server sends it without cards; the default itself is the server's)
/// SPEC-VIEW-265 (partial: the client's half: a column without `__records` is drawn folded with its count)
test("a grouped answer becomes columns: open with its cards, folded without, the unset value named None", () => {
  const { columns, length } = decodeKanbanGroups({ length: 3, groups: [
    group([7, "Ada"], 3, [{ id: 1, name: "a" }, { id: 2, name: "b" }], "owner_id"),
    group(false, 2, undefined, "owner_id"), // the unset value's column is moved to the front
    group([9, "Grace"], 0, [], "owner_id"),
  ] }, ownerField, "owner_id", 100, 400);
  expect(length).toBe(3);
  expect(columns.map(item => [item.key, item.label, item.count, item.value, item.records?.length])).toEqual([
    ["false", "None", 2, false, undefined], ["7", "Ada", 3, 7, 2], ["9", "Grace", 0, 9, 0]]);
  expect(columns[1]!.domain).toEqual([["owner_id", "=", 7]]);
});

test("a grouped answer outside its shape is refused", () => {
  const decode = (groups: unknown[], length = groups.length, limit = 100, maxCards = 400) => () => decodeKanbanGroups({ length, groups }, stateField, "state", limit, maxCards);
  expect(decode([{ ...group("draft", 1), surprise: 1 }])).toThrow("Invalid group property.");
  expect(decode([group("draft", 1, [{ name: "no id" }])])).toThrow("Invalid kanban column records.");
  expect(decode([group("draft", 1, "x" as unknown as unknown[])])).toThrow("Invalid kanban column records.");
  expect(decode([group("draft", 1, [{ id: 1 }, { id: 1 }])])).toThrow("Invalid kanban column records.");
  expect(decode([group("draft", 1, [{ id: 1 }, { id: 2 }])])).toThrow("Invalid kanban column records."); // more cards than the column counts
  expect(decode([group("draft", 1), group("done", 1)], 2, 1)).toThrow("Invalid group answer."); // more columns than asked for
  expect(decode([group("draft", 3, [{ id: 1 }, { id: 2 }, { id: 3 }])], 1, 100, 2)).toThrow("Invalid kanban column records."); // more cards than any column was asked for
  expect(decode([group("lost", 1)])).toThrow("Invalid selection value of state.");
});

// ---- the page address ----

test("the fold state round-trips through the address and names its grouping", () => {
  const state = withKanbanColumn(withKanbanColumn({ by: "owner_id", folded: [], opened: [] }, 7, true), false, false);
  expect(state).toEqual({ by: "owner_id", folded: [7], opened: [false] });
  const raw = encodeKanbanFold(state)!;
  expect(raw).toBe('{"by":"owner_id","f":[7],"o":[false]}');
  expect(decodeKanbanFold(raw)).toEqual(state);
  expect(withKanbanColumn(state, 7, false)).toEqual({ by: "owner_id", folded: [], opened: [false, 7] }); // a column is in one list only
  expect(encodeKanbanFold({ by: "state", folded: [], opened: [] })).toBeUndefined();
});

/** Fourteen 64-character keys and one of `last` characters: 1,024 characters when `last` is 60 (the same value the server's test builds). */
const long = (last: number): string => JSON.stringify({ by: "n", f: [...Array.from({ length: 14 }, (_, index) => String(index).padStart(64, "0")), "y".repeat(last)], o: [] });
test("a malformed or oversized fold state is refused", () => {
  for (const raw of ["", "[]", "{}", '{"by":"state","f":[],"o":[],"x":1}', '{"by":"State","f":[],"o":[]}', '{"by":"state:hour","f":[],"o":[]}',
    '{"by":"state","f":[null],"o":[]}', '{"by":"state","f":[1.5],"o":[]}', '{"by":"state","f":["a","a"],"o":[]}', '{"by":"state","f":["a"],"o":["a"]}',
    JSON.stringify({ by: "state", f: ["x".repeat(65)], o: [] }), JSON.stringify({ by: "state", f: Array.from({ length: 65 }, (_, index) => index), o: [] }),
    JSON.stringify({ by: "a".repeat(129), f: [1], o: [] }), long(61),
    // a text key an address cannot carry
    JSON.stringify({ by: "name", f: ["a//b"], o: [] }), JSON.stringify({ by: "name", f: ["a\u0085b"], o: [] }), JSON.stringify({ by: "name", f: ['a"b'], o: [] }), JSON.stringify({ by: "name", f: ["a\\b"], o: [] }),
    // other spellings of the same state: only the text this client writes is admitted (the server's policy agrees)
    '{"by":"state","f":[1.0],"o":[]}', '{"by":"state","f":[-0],"o":[]}', '{"by":"state","f":[1e2],"o":[]}', '{"f":[1],"o":[],"by":"state"}', '{"by":"state", "f":[1],"o":[]}', '{"by":"state","f":[1],"o":[],"o":[]}']) {
    expect(validKanbanFold(raw), raw).toBe(false);
    expect(() => decodeKanbanFold(raw)).toThrow("Invalid kanban column state.");
  }
  expect(validKanbanFold('{"by":"day:week","f":["2026-11-09"],"o":[true,3]}')).toBe(true);
  expect(validKanbanFold(JSON.stringify({ by: "a".repeat(128), f: [1], o: [] }))).toBe(true);
  expect(long(60)).toHaveLength(1024);
  expect(validKanbanFold(long(60))).toBe(true);
  expect(validKanbanFold(JSON.stringify({ by: "name", f: ["é".repeat(64)], o: [] }))).toBe(true); // length is counted as the string's, not in bytes
  expect(() => encodeKanbanFold({ by: "state", folded: Array.from({ length: 65 }, (_, index) => index), opened: [] })).toThrow("Too many columns are folded or opened to keep in the page address.");
});

test("the route admits kanban_fold with its closed shape only", () => {
  const raw = '{"by":"state","f":["draft"],"o":[]}';
  const url = routeUrl({ action: "20", view_type: "kanban", kanban_fold: raw });
  expect(parseRoute(new URL(url, "https://erp.example.test")).kanban_fold).toBe(raw);
  expect(() => routeUrl({ action: "20", view_type: "kanban", kanban_fold: '{"by":"state"}' })).toThrow("Invalid navigation parameter: kanban_fold.");
});

test("opening info names only what the user changed, and the cards a column had when a record was opened", () => {
  expect(kanbanOpeningInfo(undefined, 40)).toBeUndefined();
  expect(kanbanOpeningInfo({ by: "state", folded: [], opened: [] }, 40)).toBeUndefined();
  expect(kanbanOpeningInfo({ by: "state", folded: ["draft"], opened: [false, 7] }, 40)).toEqual([
    { value: "draft", folded: true }, { value: false, folded: false, limit: 40, offset: 0 }, { value: 7, folded: false, limit: 40, offset: 0 }]);
  // Coming back from a record: a column that had loaded more keeps that many; a folded one stays folded.
  expect(kanbanOpeningInfo({ by: "state", folded: ["draft"], opened: [] }, 40, new Map<string | number | boolean, number>([["done", 120], ["draft", 80], [3, 20]]))).toEqual([
    { value: "draft", folded: true }, { value: "done", folded: false, limit: 120, offset: 0 }]);
});

// ---- the board ----

const plain = (item: { name: string }, record: Record<string, unknown>) => {
  const node = document.createElement("span");
  node.textContent = String(record[item.name] ?? "");
  return { node, dispose: () => undefined };
};
const records = (from: number, count: number) => Array.from({ length: count }, (_, index) => ({ id: from + index, name: `Note ${from + index}`, state: "draft" }));
const state = (key: string, label: string, count: number, extra: Partial<KanbanColumnState> = {}): KanbanColumnState =>
  ({ key, label, count, folded: false, records: [], busy: false, ...extra });
const board = (columns: KanbanColumnState[], extra: { maxCards?: number; open?: (id: number) => void } = {}) => {
  const calls: string[] = [];
  const rendered = renderKanbanBoard({
    columns, layout: kanbanLayout(view()), renderField: plain, maxCards: extra.maxCards ?? 400, ...(extra.open ? { open: extra.open } : {}),
    onToggle: item => calls.push(`toggle ${item.key}`), onLoadMore: item => calls.push(`more ${item.key}`), onRetry: item => calls.push(`retry ${item.key}`),
  });
  document.body.append(rendered.node);
  return { rendered, calls, columns: () => [...rendered.node.querySelectorAll<HTMLElement>(".kanban-column")] };
};
afterEach(() => document.body.replaceChildren());

/// SPEC-VIEW-266 (partial: title and count on open and folded columns, zero included; the progress bar row is M4.KANBAN-CLIENT-COLUMNS.PROGRESS and `falsy_value_label` is not built)
/// SPEC-VIEW-267 (partial: a folded column is one control with its title and count and draws no cards)
test("a column shows its title and count; a folded column is one control with both; an empty open column says so", async () => {
  const { rendered, calls, columns } = board([
    state("a", "Draft", 3, { records: records(1, 3) }), state("b", "Done", 12, { folded: true }), state("c", "Archived", 0)]);
  const [draft, done, archived] = columns();
  expect(draft!.getAttribute("aria-label")).toBe("Draft, 3 records");
  expect(draft!.querySelector(".kanban-column-title")!.textContent).toBe("Draft");
  expect(draft!.querySelector(".kanban-column-count")!.textContent).toBe("3");
  expect(draft!.querySelectorAll(".kanban-card")).toHaveLength(3);
  const fold = draft!.querySelector<HTMLButtonElement>(".kanban-column-fold")!;
  expect([fold.getAttribute("aria-expanded"), fold.getAttribute("aria-label")]).toEqual(["true", "Fold column Draft"]);
  expect(done!.classList.contains("is-folded")).toBe(true);
  expect(done!.querySelectorAll(".kanban-card")).toHaveLength(0);
  const unfold = done!.querySelector<HTMLButtonElement>(".kanban-column-unfold")!;
  expect([unfold.getAttribute("aria-expanded"), unfold.getAttribute("aria-label"), unfold.textContent]).toEqual(["false", "Unfold column Done, 12 records", "Done12"]);
  expect(archived!.querySelector(".kanban-column-empty")!.textContent).toBe("No records");
  expect(archived!.querySelector(".kanban-load-more")).toBeNull();
  await auditComponent(rendered.node);
  fold.click(); unfold.click();
  expect(calls).toEqual(["toggle a", "toggle b"]);
  rendered.dispose();
});

test("load more shows what is left, a column at its bound says so, and a refresh redraws one column only", () => {
  const columnsState = [state("a", "Draft", 30, { records: records(1, 20) }), state("b", "Done", 500, { records: records(100, 40) })];
  const { rendered, calls, columns } = board(columnsState, { maxCards: 40 });
  const more = columns()[0]!.querySelector<HTMLButtonElement>(".kanban-load-more")!;
  expect(more.textContent).toBe("Load more (10 left)");
  expect(columns()[1]!.querySelector(".kanban-load-more")).toBeNull();
  expect(columns()[1]!.querySelector(".kanban-column-bound")!.textContent).toBe("Showing the first 40 of 500 records. Narrow the search to see the others.");
  more.focus(); more.click();
  expect(calls).toEqual(["more a"]);
  const untouched = columns()[1]!;
  columnsState[0]!.records.push(...records(21, 10));
  rendered.refresh("a");
  expect(columns()[0]!.querySelectorAll(".kanban-card")).toHaveLength(30);
  expect(columns()[0]!.querySelector(".kanban-load-more")).toBeNull();
  expect((document.activeElement as HTMLElement).dataset["id"]).toBe("21"); // focus goes to the first card that arrived
  expect(columns()[1]).toBe(untouched);
  rendered.dispose();
});

test("a column whose read failed says so with a retry and keeps its cards; the other columns are untouched; a busy column is marked", () => {
  const columnsState = [state("a", "Draft", 30, { records: records(1, 20), error: "Unable to load these records." }), state("b", "Done", 2, { records: records(50, 2) }), state("c", "Late", 4, { busy: true })];
  const { rendered, calls, columns } = board(columnsState);
  const alert = columns()[0]!.querySelector<HTMLElement>(".kanban-column-error")!;
  expect(alert.getAttribute("role")).toBe("alert");
  expect(alert.querySelector("p")!.textContent).toBe("Unable to load these records.");
  expect(columns()[0]!.querySelectorAll(".kanban-card")).toHaveLength(20);
  expect(columns()[0]!.querySelector(".kanban-load-more")).toBeNull();
  expect(columns()[1]!.querySelectorAll(".kanban-card")).toHaveLength(2);
  expect(columns()[1]!.querySelector(".kanban-column-error")).toBeNull();
  expect(columns()[2]!.querySelector(".kanban-column-body")!.getAttribute("aria-busy")).toBe("true");
  expect(columns()[2]!.querySelector(".kanban-column-empty")).toBeNull();
  alert.querySelector<HTMLButtonElement>("button")!.click();
  expect(calls).toEqual(["retry a"]);
  rendered.dispose();
});

/// SPEC-VIEW-147 (partial: Up and Down inside a column, Left and Right to the first card of the neighbouring column that shows cards, folded and empty columns passed over; Space selection is SPEC-VIEW-148, not built)
/// SPEC-VIEW-149 (partial: Enter on a focused card of a column opens it; quick create is M4.KANBAN-CLIENT-COLUMNS.QUICK-CREATE)
test("the keyboard: one card takes Tab, Up and Down stay in the column, Left and Right skip folded and empty columns, Enter opens", () => {
  const opened: number[] = [];
  const { rendered } = board([
    state("a", "Draft", 3, { records: records(1, 3) }), state("b", "Folded", 5, { folded: true }), state("c", "Empty", 0), state("d", "Done", 2, { records: records(10, 2) })], { open: id => opened.push(id) });
  const cards = [...rendered.node.querySelectorAll<HTMLElement>(".kanban-card")];
  expect(cards.map(card => card.tabIndex)).toEqual([0, -1, -1, -1, -1]);
  const press = (key: string) => (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  const focused = () => (document.activeElement as HTMLElement).dataset["id"];
  cards[0]!.focus();
  press("ArrowDown"); press("ArrowDown"); expect(focused()).toBe("3");
  press("ArrowDown"); expect(focused()).toBe("3"); // the last card of the column stays
  press("ArrowRight"); expect(focused()).toBe("10"); // the first card of the next column that shows cards
  press("ArrowRight"); expect(focused()).toBe("10"); // no column further right
  press("ArrowDown"); expect(focused()).toBe("11");
  expect(cards.map(card => card.tabIndex)).toEqual([-1, -1, -1, -1, 0]);
  press("ArrowUp"); expect(focused()).toBe("10");
  press("ArrowLeft"); expect(focused()).toBe("1");
  press("End"); expect(focused()).toBe("3");
  press("Home"); expect(focused()).toBe("1");
  press("Enter");
  cards[4]!.click();
  expect(opened).toEqual([1, 11]);
  rendered.dispose();
});

// ---- through the application ----

const american = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S", ...american },
  csrf: { logout: "test-token-not-secret" }
};
const listView = { identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field("name", "char", "Name")] };
const searchView = { identity: { kind: "stored", id: 4 }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [{ kind: "search", children: [] }] };
const formView = { identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field("name", "char", "Name")] };
// 45 notes: 30 draft, 15 done (every third).
const fixtureNotes = Array.from({ length: 45 }, (_, index) => ({ id: index + 1, name: `Note ${index + 1}`, state: index % 3 ? "draft" : "done" }));
let all = fixtureNotes.map(row => ({ ...row }));
let options: { root?: Root; failWrite?: boolean; denyArchive?: boolean; failColumn?: boolean; maxColumns?: number; maxCards?: number } = {};
beforeEach(() => {
  all = fixtureNotes.map(row => ({ ...row }));
  options = { root: { default_group_by: ref("state") } };
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const node = document.createElement("div"); node.setAttribute(`data-control-${key}`, ""); root.append(node); }
    return root;
  } }]);
});
let app: HumanApp | undefined;
afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); history.replaceState(null, "", "/"); document.body.replaceChildren(); });
const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
/** The domains this fixture understands: a `name ilike` term and a `state =` term (a column's own domain). */
const keep = (domain: unknown[]) => all.filter(record => domain.every(term => !Array.isArray(term) || (term[0] === "name" ? record.name.toLowerCase().includes(String(term[2]).toLowerCase()) : term[0] === "state" ? record.state === term[2] : true)));

async function startApp(path: string) {
  history.replaceState(null, "", path);
  const calls: { method: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const method = url.split("/").pop()!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ method, body });
    const description = { ...kanbanDescription(options.root), views: [listView, kanbanDescription(options.root).views[0]!, formView, searchView] };
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Notes", model: "t.note", target: "current", views: [{ id: 2, type: "list" }, { id: 7, type: "kanban" }, { id: 3, type: "form" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: description } : action);
    }
    if (method === "get_views") return reply(description);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true }, state: { string: "State", type: "selection", groupable: true, selection: [["draft", "Draft"], ["done", "Done"]] }, day: { string: "Day", type: "date", groupable: true } });
    if (method === "erp_read_values") return reply({ wire_major: 1, values: (body["keys"] as string[]).map(key => ({ key, value: key === "web.kanban_max_columns" ? options.maxColumns ?? 100 : options.maxCards ?? 400 })) });
    if (method === "has_access") return reply(!options.denyArchive);
    if (method === "action_archive") {
      all = all.filter(row => !(body["ids"] as number[]).includes(row.id));
      return reply(true);
    }
    if (method === "name_create") {
      const id = all.length + 1, name = String(body["name"]);
      all.push({ id, name, state: String((body["context"] as Record<string, unknown>)["default_state"]) });
      return reply([id, name]);
    }
    if (method === "write") {
      if (options.failWrite) return reply({ message: "Record is read-only." }, 403);
      const ids = body["ids"] as number[], vals = body["vals"] as { state: string };
      for (const row of all.filter(row => ids.includes(row.id))) row.state = vals.state;
      return reply(true);
    }
    if (method === "read_progress_bar") {
      const kept = keep(body["domain"] as unknown[]);
      return reply(Object.fromEntries(["draft", "done"].map(state => [state, { [state]: kept.filter(row => row.state === state).length }])));
    }
    if (method === "web_read_group") {
      // The server's rule (crates/erp-orm/src/web/groups.rs `open_groups`): an entry of opening_info decides its column, the others open.
      const info = (body["opening_info"] ?? []) as { value: unknown; folded: boolean; limit?: number }[];
      const kept = keep(body["domain"] as unknown[]), spec = (body["groupby"] as string[])[0]!;
      // Every note is of one month: a date grouping answers one interval with its start and its label.
      if (spec === "day:month") return reply({ length: 1, groups: [{ "day:month": ["2026-11-01", "November 2026"], __count: kept.length, __extra_domain: [["day", ">=", "2026-11-01"], ["day", "<", "2026-12-01"]], __records: kept.slice(0, Number(body["unfold_read_default_limit"])) }] });
      const groups = (spec === "state" ? ["draft", "done"] : [...new Set(kept.map(record => record.name))]).map(value => {
        const rows = kept.filter(record => (spec === "state" ? record.state : record.name) === value), entry = info.find(item => item.value === value);
        return { [spec]: value, ...((body["aggregates"] as string[]).includes("volume:sum") ? {"volume:sum": rows.length * 5} : {}), __count: rows.length, __extra_domain: [[spec, "=", value]], ...(body["auto_unfold"] !== true || entry?.folded ? {} : { __records: rows.slice(0, entry?.limit ?? Number(body["unfold_read_default_limit"])) }) };
      }).filter(item => item.__count > 0);
      return reply({ length: groups.length, groups: groups.slice(0, Number(body["limit"])) });
    }
    if (method === "web_search_read") {
      if (options.failColumn) return reply({ message: "The server is busy." }, 500);
      const kept = keep(body["domain"] as unknown[]), offset = Number(body["offset"] ?? 0), limit = Number(body["limit"]);
      return reply({ length: kept.length, records: kept.slice(offset, offset + limit) });
    }
    if (method === "web_read") {
      const ids = body["ids"] as number[];
      return reply(all.filter(record => ids.includes(record.id)).map(record => ({ id: record.id, name: record.name })));
    }
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  return { root, of: (method: string) => calls.filter(call => call.method === method) };
}
const query = (): URLSearchParams => new URL(window.location.href).searchParams;
const columns = (root: HTMLElement) => [...root.querySelectorAll<HTMLElement>(".kanban-column")];
const cardsOf = (item: HTMLElement) => [...item.querySelectorAll<HTMLElement>(".kanban-card")];
const titles = (root: HTMLElement) => columns(root).map(item => `${item.querySelector(".kanban-column-title, .kanban-column-unfold .kanban-column-name")!.textContent} ${item.querySelector(".kanban-column-count")!.textContent}`);
const fold = (state: unknown) => encodeURIComponent(JSON.stringify(state));

/// SPEC-VIEW-043 (partial: one grouped read brings every column with its first cards; the number of columns is bounded by the setting web.kanban_max_columns, where the reference asks without a bound)
test("the arch's default grouping opens the board with one grouped read: every column, its count and its first cards", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(titles(root)).toEqual(["Draft 30", "Done 15"]);
  expect(columns(root).map(item => cardsOf(item).length)).toEqual([30, 15]);
  const read = of("web_read_group");
  expect(read).toHaveLength(1);
  expect(read[0]!.body).toMatchObject({ domain: [], groupby: ["state"], aggregates: ["__count"], limit: 100, auto_unfold: true, unfold_read_default_limit: 40 });
  expect(read[0]!.body).not.toHaveProperty("opening_info");
  expect(Object.keys(read[0]!.body["unfold_read_specification"] as object).sort()).toEqual(["name", "state"]);
  expect(of("web_search_read")).toHaveLength(0);
  expect(of("erp_read_values")[0]!.body["keys"]).toEqual(["web.kanban_max_columns", "web.kanban_max_column_cards"]);
  expect(root.querySelector(".pager-count")!.textContent).toBe("45 records");
  expect(root.querySelector(".pager-next, .pager-prev")).toBeNull(); // a board has no pages
  expect(root.querySelector(".kanban-notice")).toBeNull();
  expect(root.querySelectorAll(".search-facet-chip")).toHaveLength(0); // the default grouping is not a Group By of the search
  expect(query().has("kanban_fold")).toBe(false);
  expect(query().has("search_groupby")).toBe(false);
});

/// SPEC-VIEW-264 (partial: the search's first grouping makes the columns, and removing it returns to the arch's default grouping; the Group By menu itself is the search bar's)
test("Group By from the search groups the board by its first grouping; without it the arch's default grouping returns", async () => {
  const { root, of } = await startApp(`/erp/action-20?view_type=kanban&search_groupby=${fold(["name", "state"])}`);
  await vi.waitFor(() => expect(columns(root)).toHaveLength(45));
  expect(of("web_read_group").at(-1)!.body["groupby"]).toEqual(["name"]);
  expect([...root.querySelectorAll(".search-facet-chip")].map(item => item.textContent).join(" ")).toContain("Name");
  root.querySelector<HTMLButtonElement>(".o_facet_remove, [aria-label^='Remove']")!.click();
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(of("web_read_group").at(-1)!.body["groupby"]).toEqual(["state"]);
  expect(query().has("search_groupby")).toBe(false);
});

/// SPEC-VIEW-264 (partial: a default grouping by a date is by month)
test("a default grouping by a date groups by month, and a folded month keeps its start in the address", async () => {
  options.root = { default_group_by: ref("day") };
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(1));
  expect(of("web_read_group")[0]!.body["groupby"]).toEqual(["day:month"]);
  expect(titles(root)).toEqual(["November 2026 45"]);
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-column-fold")!.click();
  await vi.waitFor(() => expect(query().get("kanban_fold")).toBe('{"by":"day:month","f":["2026-11-01"],"o":[]}'));
});

test("a kanban without a default grouping and without Group By stays the ungrouped grid with its pager", async () => {
  options.root = {};
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(root.querySelectorAll(".kanban-grid .kanban-card")).toHaveLength(40));
  expect(of("web_read_group")).toHaveLength(0);
  expect(root.querySelector(".pager-next")).not.toBeNull();
  expect(columns(root)).toHaveLength(0);
});

/// SPEC-VIEW-045 (partial: the fold control folds an open column without a read and a folded column opens on a click; the reference's scroll reset and its small-screen rule are not asserted here)
/// SPEC-VIEW-273 (partial: folding and unfolding write `kanban_fold` to the address)
test("folding a column writes the address and reads nothing; unfolding it shows its cards again", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  const control = columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-column-fold")!;
  control.focus(); control.click();
  await vi.waitFor(() => expect(columns(root)[0]!.classList.contains("is-folded")).toBe(true));
  expect(query().get("kanban_fold")).toBe('{"by":"state","f":["draft"],"o":[]}');
  expect(cardsOf(columns(root)[0]!)).toHaveLength(0);
  expect(cardsOf(columns(root)[1]!)).toHaveLength(15);
  expect(document.activeElement).toBe(columns(root)[0]!.querySelector(".kanban-column-unfold"));
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-column-unfold")!.click();
  await vi.waitFor(() => expect(cardsOf(columns(root)[0]!)).toHaveLength(30));
  expect(query().get("kanban_fold")).toBe('{"by":"state","f":[],"o":["draft"]}');
  expect(of("web_read_group")).toHaveLength(1);
  expect(of("web_search_read")).toHaveLength(0); // its cards were still there
});

/// SPEC-VIEW-269 (partial: the grouped read is told which columns the user folded or opened; ours tells only those, from the address, where the reference names every known column)
/// SPEC-VIEW-267 (partial: a click on a folded column reads its first page under the column's domain and opens it; the scroll into view is the browser's on focus)
/// SPEC-VIEW-273 (partial: the folded columns of the address open the same board again)
test("a reload keeps the folded column: the grouped read says so, and unfolding reads that column's cards", async () => {
  const { root, of } = await startApp(`/erp/action-20?view_type=kanban&kanban_fold=${fold({ by: "state", f: ["draft"], o: [] })}`);
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(of("web_read_group")[0]!.body["opening_info"]).toEqual([{ value: "draft", folded: true }]);
  expect(columns(root)[0]!.classList.contains("is-folded")).toBe(true);
  expect(titles(root)).toEqual(["Draft 30", "Done 15"]);
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-column-unfold")!.click();
  await vi.waitFor(() => expect(cardsOf(columns(root)[0]!)).toHaveLength(30));
  expect(of("web_search_read").at(-1)!.body).toMatchObject({ domain: [["state", "=", "draft"]], offset: 0, limit: 40 });
});

/// SPEC-VIEW-273 (partial: a state that names another grouping is dropped)
test("a fold state of another grouping is dropped from the address", async () => {
  const { root, of } = await startApp(`/erp/action-20?view_type=kanban&kanban_fold=${fold({ by: "name", f: ["Note 1"], o: [] })}`);
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(of("web_read_group")[0]!.body).not.toHaveProperty("opening_info");
  expect(query().has("kanban_fold")).toBe(false);
});

/// SPEC-VIEW-046 (partial: the next step of cards arrives under the column's domain; ours reads from the offset of the cards shown, where the reference reads the column again with a larger limit)
/// SPEC-VIEW-271 (partial: the control states what is left and leaves when the column is complete; Previous and Next are hidden on a board, the count of all columns stays, which the reference does not show)
test("load more reads the next step of that column under its own domain and the control leaves when all are shown", async () => {
  options.root = { default_group_by: ref("state"), page_limit: 20 };
  const { root, of } = await startApp("/erp/action-20?view_type=kanban&search=note");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(columns(root).map(item => cardsOf(item).length)).toEqual([20, 15]);
  expect(of("web_read_group")[0]!.body).toMatchObject({ unfold_read_default_limit: 20, domain: [["name", "ilike", "note"]] });
  const more = columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-load-more")!;
  expect(more.textContent).toBe("Load more (10 left)");
  expect(columns(root)[1]!.querySelector(".kanban-load-more")).toBeNull();
  more.click();
  await vi.waitFor(() => expect(cardsOf(columns(root)[0]!)).toHaveLength(30));
  expect(of("web_search_read").at(-1)!.body).toMatchObject({ domain: [["name", "ilike", "note"], ["state", "=", "draft"]], offset: 20, limit: 20 });
  expect(columns(root)[0]!.querySelector(".kanban-load-more")).toBeNull();
  expect(root.querySelector(".pager-count")!.textContent).toBe("45 records");
});

/// SPEC-VIEW-274 (partial: a failed load more; a failed unfold takes the same path and is not asserted here)
test("a failed read of one column stays in that column with a retry; the other column keeps its cards", async () => {
  options.root = { default_group_by: ref("state"), page_limit: 20 };
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  options.failColumn = true;
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-load-more")!.click();
  await vi.waitFor(() => expect(columns(root)[0]!.querySelector(".kanban-column-error")).not.toBeNull());
  expect(columns(root)[0]!.querySelector(".kanban-column-error p")!.textContent).toBe("Unable to load these records.");
  expect(columns(root).map(item => cardsOf(item).length)).toEqual([20, 15]);
  expect(columns(root)[1]!.querySelector(".kanban-column-error")).toBeNull();
  options.failColumn = false;
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-column-error button")!.click();
  await vi.waitFor(() => expect(cardsOf(columns(root)[0]!)).toHaveLength(30));
  expect(columns(root)[0]!.querySelector(".kanban-column-error")).toBeNull();
});

/// SPEC-VIEW-272 (partial: the client's half: it asks for at most the setting's columns and refuses a larger grouping by name; the declaration is tested in addons/web/src/kanban_settings.rs)
test("a board with more columns than the setting allows is refused by name, never drawn in part", async () => {
  options.maxColumns = 1;
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(root.textContent).toContain("This grouping has 2 columns; a board shows at most 1"));
  expect(root.textContent).toContain("Kanban view: most columns a board may show");
  expect(of("web_read_group")[0]!.body["limit"]).toBe(1);
  expect(columns(root)).toHaveLength(0);
});

/// SPEC-VIEW-272 (partial: a setting value past the declared hard bound is refused by name instead of used)
test("a kanban setting outside its hard bound is refused by name", async () => {
  options.maxColumns = 1001;
  await expect(startApp("/erp/action-20?view_type=kanban")).rejects.toMatchObject({ kind: "server", message: "Invalid kanban setting web.kanban_max_columns." });
  expect(vi.mocked(fetch).mock.calls.some(([url]) => String(url).endsWith("/web_read_group"))).toBe(false); // the board is not read on a guess
});

/// SPEC-VIEW-272 (partial: the client's half of the card bound: a column at the bound says how many it shows instead of loading more)
test("a column stops loading at the setting's bound and says how many it shows", async () => {
  options.root = { default_group_by: ref("state"), page_limit: 20 };
  options.maxCards = 20;
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(columns(root)[0]!.querySelector(".kanban-load-more")).toBeNull();
  expect(columns(root)[0]!.querySelector(".kanban-column-bound")!.textContent).toBe("Showing the first 20 of 30 records. Narrow the search to see the others.");
});

/// SPEC-VIEW-149 (partial: a click on a card of a column opens its form; the way back shows the board as it was left)
/// SPEC-VIEW-270 (partial: the way back keeps the folded column, the cards that were loaded and the focused card; scroll offsets are restored by the application and asserted in the browser fixture only for focus)
test("a card opens its record; the way back keeps the folded column, the cards that were loaded and the card in focus", async () => {
  options.root = { default_group_by: ref("state"), page_limit: 20 };
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  columns(root)[1]!.querySelector<HTMLButtonElement>(".kanban-column-fold")!.click();
  columns(root)[0]!.querySelector<HTMLButtonElement>(".kanban-load-more")!.click();
  await vi.waitFor(() => expect(cardsOf(columns(root)[0]!)).toHaveLength(30));
  const last = cardsOf(columns(root)[0]!).at(-1)!, id = last.dataset["id"]!;
  last.click();
  await vi.waitFor(() => expect(query().get("view_type")).toBe("form"));
  expect(query().get("id")).toBe(id);
  const crumb = [...root.querySelectorAll<HTMLElement>("[data-control-breadcrumbs] a, [data-control-breadcrumbs] button")].find(item => item.textContent === "Notes")!;
  crumb.click();
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(query().get("kanban_fold")).toBe('{"by":"state","f":["done"],"o":[]}');
  expect(of("web_read_group").at(-1)!.body["opening_info"]).toEqual([{ value: "done", folded: true }, { value: "draft", folded: false, limit: 30, offset: 0 }]);
  expect(cardsOf(columns(root)[0]!)).toHaveLength(30);
  expect(columns(root)[1]!.classList.contains("is-folded")).toBe(true);
  expect((document.activeElement as HTMLElement).dataset["id"]).toBe(id);
});

test("the form opened from a column steps through the cards the board shows, column by column", async () => {
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  const draft = cardsOf(columns(root)[0]!);
  draft.at(-1)!.click();
  await vi.waitFor(() => expect(query().get("id")).toBe(draft.at(-1)!.dataset["id"]));
  root.querySelector<HTMLButtonElement>(".form-pager .pager-next, .form-pager [aria-label='Next record']")!.click();
  await vi.waitFor(() => expect(query().get("id")).toBe("1")); // the first done note: the next column's first card
});

test("a switch between the list and the kanban carries the grouping, not the list's open groups", async () => {
  options.root = {};
  const { root, of } = await startApp(`/erp/action-20?search_groupby=${fold({ v: 1, by: ["state"], offset: 0, order: "", open: [] })}`);
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="kanban view"]')!.click();
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(query().get("search_groupby")).toBe('["state"]');
  expect(of("web_read_group").at(-1)!.body["groupby"]).toEqual(["state"]);
});

test("the keyboard moves a card, quick creation applies its column default, and a progress filter reads one column", async () => {
  options.root!.progress = true;
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(of("read_progress_bar")).toHaveLength(1);
  const draft = () => columns(root)[0]!, done = () => columns(root)[1]!;
  const select = cardsOf(draft())[0]!.querySelector("select")!;
  select.value = done().dataset["key"]!; select.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(titles(root)).toEqual(["Draft 29","Done 16"]));
  expect(of("write")[0]!.body).toMatchObject({ ids: [2], vals: { state: "done" } });
  [...done().querySelectorAll("button")].find(node => node.textContent === "Add card")!.click();
  const form = done().querySelector("form")!; form.querySelector("input")!.value = "New done card"; form.requestSubmit();
  await vi.waitFor(() => expect(titles(root)).toEqual(["Draft 29","Done 17"]));
  expect(of("name_create")[0]!.body).toMatchObject({ name: "New done card", context: { default_state: "done" } });
  const before = of("web_read_group").length;
  done().querySelector<HTMLButtonElement>(".kanban-segment.tone-success")!.click();
  await vi.waitFor(() => expect(done().querySelector(".kanban-segment.tone-success")!.getAttribute("aria-pressed")).toBe("true"));
  expect(of("web_search_read").at(-1)!.body).toMatchObject({ offset: 0, domain: [["state","=","done"],["state","=","done"]] });
  expect(of("web_read_group")).toHaveLength(before);
});

test("a refused drag keeps the card, scroll position and keyboard focus in its original column", async () => {
  options.failWrite = true;
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  const board = root.querySelector<HTMLElement>(".kanban-board")!, card = cardsOf(columns(root)[0]!)[0]!;
  board.scrollLeft = 13; board.scrollTop = 8;
  card.dispatchEvent(new Event("dragstart", { bubbles: true }));
  columns(root)[1]!.dispatchEvent(new Event("drop", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(of("write")).toHaveLength(1));
  expect(titles(root)).toEqual(["Draft 30","Done 15"]);
  expect(cardsOf(columns(root)[0]!)[0]).toBe(card);
  expect([board.scrollLeft,board.scrollTop]).toEqual([13,8]);
  const dismiss = await vi.waitFor(() => {
    const control = document.querySelector<HTMLButtonElement>("dialog[open] .message-dialog-close");
    expect(control).not.toBeNull();
    return control!;
  });
  dismiss.click();
  await vi.waitFor(() => expect(document.activeElement).toBe(card));
});

test("read-only board omits card mutation controls while folding remains available", async () => {
  Object.assign(options.root!, { create: false, edit: false });
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(root.querySelector(".kanban-column select")).toBeNull();
  expect(columns(root)[0]!.textContent).not.toContain("Add card");
  expect(columns(root)[0]!.querySelector(".kanban-column-fold")).not.toBeNull();
});

test("a progress sum uses the grouped read and the residual segment uses the complement of the configured colors", async () => {
  Object.assign(options.root!, { progress: true, sum: true, uncolored: true });
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  expect(of("web_read_group")[0]!.body["aggregates"]).toEqual(["__count","volume:sum"]);
  const done = columns(root)[1]!;
  expect(done.textContent).toContain("Volume: 75");
  const other = [...done.querySelectorAll("button")].find(node => node.textContent === "Other (15)")!;
  other.click();
  await vi.waitFor(() => expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["state","=","done"],"!",["state","in",["draft"]]]));
});

test("column archiving checks the full visible selection, refuses unauthorized records, and confirms an allowed batch", async () => {
  options.root!.archive = true; options.denyArchive = true;
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(columns(root)).toHaveLength(2));
  const archive = () => [...columns(root)[0]!.querySelectorAll("button")].find(node => node.textContent === "Archive column records")!;
  archive().click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")?.textContent).toContain("cannot archive"));
  expect(of("action_archive")).toHaveLength(0);
  document.querySelector<HTMLButtonElement>("dialog .message-dialog-close")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")).toBeNull());
  options.denyArchive = false; archive().click();
  await vi.waitFor(() => expect(document.querySelector("dialog[open]")?.textContent).toContain("Archive 30 records"));
  [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(btn => btn.textContent === "Archive")!.click();
  await vi.waitFor(() => expect(titles(root)).toEqual(["Done 15"]));
  expect(of("has_access").at(-1)!.body).toMatchObject({ operation: "write", ids: fixtureNotes.filter(row => row.state === "draft").map(row => row.id) });
});

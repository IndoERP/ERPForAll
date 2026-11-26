/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Kanban cards, ungrouped: the description admission, the card layout, the keyboard and the
// application path (view switcher, search, pager, open and way back, empty and failed reads, dashboard cards in the address).
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { describeViews, type View } from "../src/client/metadata.js";
import { initials, kanbanLayout, renderKanbanCards } from "../src/client/kanban.js";
import { auditComponent } from "../../../../web/testing/axe.js";

const column = (type: string, label: string, extra: Record<string, unknown> = {}) =>
  ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null, ...extra });
const modelFields = {
  name: column("char", "Name"),
  email: column("char", "Email"),
  state: column("selection", "State", { selection: [["draft", "Draft"], ["done", "Done"]] }),
  size: column("integer", "Size"),
  secret: column("char", "Secret")
};
const ref = (name: string) => ({ model: "t.note", name });
const modifiers = { readonly: null, required: null, invisible: null };
const field = (name: string, widget: string, label: string, extra: Record<string, unknown> = {}) => ({ kind: "field", force_save: false, field: ref(name), widget, options: {}, modifiers, label, ...extra });
type Root = { page_limit?: number; default_group_by?: unknown; can_open?: boolean; create?: boolean };
/** A kanban description the way the server sends it (crates/erp-views/src/wire.rs `ViewNode::Kanban`). */
const kanbanNodes = (root: Root = {}) => [
  { kind: "kanban", children: [1, 9], quick_create: true, records_draggable: true, groups_draggable: true, archivable: true, can_open: root.can_open ?? true,
    ...(root.page_limit ? { page_limit: root.page_limit } : {}), ...(root.default_group_by ? { default_group_by: root.default_group_by } : {}), ...(root.create === undefined ? {} : { create: root.create }) },
  { kind: "kanban_card", children: [2, 3, 5, 7] },
  field("name", "char", "Name"),
  { kind: "kanban_section", region: "main", children: [4] },
  field("email", "email", "Email"),
  { kind: "kanban_section", region: "aside", children: [6] },
  field("name", "char", "Name"),
  { kind: "kanban_section", region: "footer", children: [8, 10] },
  field("state", "selection", "State"),
  { kind: "kanban_progressbar", field: ref("state"), colors: [{ value: "done", tone: "success" }] },
  field("secret", "char", "Secret", { modifiers: { ...modifiers, invisible: { schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value: true } }] } } })
];
const kanbanDescription = (root?: Root) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.note", fields: { "t.note": modelFields }, notices: [],
  views: [{ identity: { kind: "stored", id: 7 }, type: "kanban", schema_major: 1, renderer: "web.kanban", root: 0, nodes: kanbanNodes(root) }]
});
const view = (root?: Root): View => describeViews(kanbanDescription(root), "t.note", "r1").views.get("kanban")!;

// ---- description and layout ----

test("a kanban description is admitted with its flags, its card regions and its progress bar declaration", () => {
  const kanban = view({ page_limit: 12 });
  const root = kanban.nodes[0]!;
  expect(root.kind).toBe("kanban");
  if (root.kind !== "kanban") return;
  expect([root.quickCreate, root.recordsDraggable, root.groupsDraggable, root.archivable, root.canOpen]).toEqual([true, true, true, true, true]);
  expect(root.defaultGroupBy).toBeNull();
  expect(kanban.pageLimit).toBe(12);
  const bar = root.children.find(child => child.kind === "kanban_progressbar");
  expect(bar).toMatchObject({ kind: "kanban_progressbar", colors: [{ value: "done", tone: "success" }], sumField: null, help: null });
  expect(kanban.fields.map(item => item.name)).toEqual(["name", "email", "state", "secret"]); // a field shown twice is read once
});

test("a kanban description outside its closed shape is refused", () => {
  const broken = (change: (nodes: Record<string, unknown>[]) => void) => {
    const description = kanbanDescription();
    change(description.views[0]!.nodes as Record<string, unknown>[]);
    return () => describeViews(description, "t.note", "r1");
  };
  expect(broken(nodes => { nodes[0]!["class"] = "x"; })).toThrow("Unknown view node member: class.");
  expect(broken(nodes => { (nodes[3] as { region: string }).region = "header"; })).toThrow("Unsupported kanban card region.");
  expect(broken(nodes => { (nodes[9] as { colors: unknown[] }).colors = [{ value: "done", tone: "300" }]; })).toThrow("Unsupported progress bar tone.");
  expect(broken(nodes => { nodes[0]!["page_limit"] = 0; })).toThrow("Kanban page limit must be a positive browser-safe integer.");
  expect(broken(nodes => { (nodes[1] as { children: number[] }).children = [2, 3, 3]; })).toThrow();
  expect(broken(nodes => { (nodes[7] as { children: number[] }).children = [3]; })).toThrow(); // a region inside a region
});

test("the card layout: the first field is the title, main fields are lines, the aside is the tile, a statically hidden field is left out", () => {
  const layout = kanbanLayout(view());
  expect(layout.title?.name).toBe("name");
  expect(layout.lines.map(item => item.name)).toEqual(["email"]);
  expect(layout.aside?.name).toBe("name");
  expect(layout.footer.map(item => item.name)).toEqual(["state"]); // `secret` is invisible
  expect(initials("Ada Lovelace")).toBe("AL");
  expect(initials("  émile  ")).toBe("É");
  expect(initials([3, "Grace Brewster Hopper"])).toBe("GH");
  expect(initials(false)).toBe("");
});

const plain = (item: { name: string }, record: Record<string, unknown>) => {
  const node = document.createElement("span");
  node.textContent = String(record[item.name] ?? "");
  return { node, dispose: () => undefined };
};
const rows = [{ id: 1, name: "Ada Lovelace", email: "ada@example.test", state: "draft" }, { id: 2, name: "Grace Hopper", email: "", state: "done" }, { id: 3, name: "Alan Turing", email: "", state: "done" }];

/// (partial: ungrouped cards; Left, Right, Home and End move a roving focus; Up and Down follow the rows of the layout, not measurable in jsdom)
/// (partial: Enter on a focused card opens it; quick create is)
test("the card grid: one focusable card per record, arrows move the focus, Enter and a click open, a click on a link inside does not", async () => {
  const opened: number[] = [];
  const grid = renderKanbanCards({ layout: kanbanLayout(view()), records: rows, renderField: plain, open: id => opened.push(id) });
  document.body.append(grid.node);
  const cards = [...grid.node.querySelectorAll<HTMLElement>(".kanban-card")];
  expect(cards.map(card => card.tabIndex)).toEqual([0, -1, -1]);
  expect(cards.map(card => card.getAttribute("aria-label"))).toEqual(["Ada Lovelace", "Grace Hopper", "Alan Turing"]);
  expect(cards[0]!.querySelector(".kanban-card-tile")!.textContent).toBe("AL");
  expect(cards[0]!.querySelector(".kanban-card-tile")!.getAttribute("aria-hidden")).toBe("true");
  const press = (key: string) => (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  cards[0]!.focus();
  press("ArrowRight");
  expect(document.activeElement).toBe(cards[1]);
  expect(cards.map(card => card.tabIndex)).toEqual([-1, 0, -1]);
  press("End");
  expect(document.activeElement).toBe(cards[2]);
  press("ArrowRight"); // the last card stays
  expect(document.activeElement).toBe(cards[2]);
  press("Home");
  expect(document.activeElement).toBe(cards[0]);
  await auditComponent(grid.node); // throws on a serious or critical finding
  press("Enter");
  expect(opened).toEqual([1]);
  cards[1]!.querySelector<HTMLElement>(".kanban-card-title")!.click();
  expect(opened).toEqual([1, 2]);
  const link = document.createElement("a"); link.href = "#"; cards[2]!.append(link);
  link.addEventListener("click", event => event.preventDefault());
  link.click();
  expect(opened).toEqual([1, 2]);
  grid.dispose();
  document.body.replaceChildren();
});

test("cards that cannot open are not marked or activated as links", () => {
  const grid = renderKanbanCards({ layout: kanbanLayout(view()), records: rows, renderField: plain });
  const card = grid.node.querySelector<HTMLElement>(".kanban-card")!;
  expect(card.classList.contains("kanban-card-open")).toBe(false);
  card.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  card.click(); // nothing to call: no error either
  grid.dispose();
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
const dashboardRevision = "b".repeat(64);
const listView = (dashboard: boolean) => ({ identity: { kind: "stored", id: 2 }, type: "list", schema_major: 1, renderer: "web.list", root: 0,
  nodes: [{ kind: "list", children: dashboard ? [1, 2] : [1] }, field("name", "char", "Name"),
    ...(dashboard ? [{ kind: "dashboard", version: 1, revision: dashboardRevision, items: [{ kind: "count", name: "done", label: "Done notes", domain: null, filter: "done", color: "info" }] }] : [])] });
const searchView = { identity: { kind: "stored", id: 4 }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [{ kind: "search", children: [] }] };
const formView = { identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field("name", "char", "Name")] };
const all = Array.from({ length: 45 }, (_, index) => ({ id: index + 1, name: index === 0 ? "Ada Lovelace" : `Note ${index + 1}`, email: `n${index + 1}@example.test`, state: index % 3 ? "draft" : "done", size: index, secret: "x" }));
let options: { root?: Root; dashboard?: boolean; fail?: boolean; none?: boolean } = {};
beforeEach(() => {
  options = {};
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
/** The domains this fixture understands: `[]`, a `name ilike` term and the dashboard card's `state = done`. */
const keep = (domain: unknown[]) => all.filter(record => domain.every(term => !Array.isArray(term) || (term[0] === "name" ? record.name.toLowerCase().includes(String(term[2]).toLowerCase()) : term[0] === "state" ? record.state === term[2] : true)));

async function startApp(path: string) {
  history.replaceState(null, "", path);
  const calls: { method: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const method = url.split("/").pop()!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ method, body });
    const description = { ...kanbanDescription(options.root), views: [listView(options.dashboard === true), kanbanDescription(options.root).views[0]!, formView, searchView] };
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Notes", model: "t.note", target: "current", views: [{ id: 2, type: "list" }, { id: 7, type: "kanban" }, { id: 3, type: "form" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: description } : action);
    }
    if (method === "get_views") return reply(description);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true }, state: { string: "State", type: "selection", groupable: true, selection: [["draft", "Draft"], ["done", "Done"]] } });
    if (method === "erp_read_dashboard") return reply({ version: 1, hidden: false, revision: dashboardRevision, period: "week", periods: [{ id: "week", label: "Week", kind: "rolling", days: 7 }], values: [{ name: "done", value: "15", domain: [["state", "=", "done"]] }] });
    if (method === "web_search_read") {
      if (options.fail) return reply({ message: "The server is busy." }, 500);
      const kept = options.none ? [] : keep(body["domain"] as unknown[]);
      const offset = Number(body["offset"] ?? 0), limit = Number(body["limit"]);
      return reply({ length: kept.length, records: kept.slice(offset, offset + limit).map(({ secret: _secret, size: _size, ...record }) => record) });
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
const cards = (root: HTMLElement) => [...root.querySelectorAll<HTMLElement>(".kanban-card")];

test("the kanban is reached from the view switcher and reads one page of 40 cards with the card fields", async () => {
  const { root, of } = await startApp("/erp/action-20");
  expect([...root.querySelectorAll(".view-btn")].map(button => button.getAttribute("aria-label"))).toEqual(["list view", "kanban view", "form view"]);
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="kanban view"]')!.click();
  await vi.waitFor(() => expect(cards(root)).toHaveLength(40));
  expect(query().get("view_type")).toBe("kanban");
  const read = of("web_search_read").at(-1)!.body;
  expect(read).toMatchObject({ domain: [], offset: 0, limit: 40 });
  expect(read["context"]).toMatchObject({ bin_size: true }); // files come as sizes, a widget reads the one it draws
  expect(Object.keys(read["specification"] as object).sort()).toEqual(["email", "name", "secret", "state"]);
  expect(root.querySelector('.view-btn[aria-label="kanban view"]')!.getAttribute("aria-current")).toBe("page");
  expect(root.querySelector(".pager-count")!.textContent).toBe("1-40 / 45");
  expect(cards(root)[0]!.querySelector(".kanban-card-title")!.textContent).toContain("Ada Lovelace");
  expect(cards(root)[0]!.querySelector('[data-field="secret"]')).toBeNull();
  expect(root.querySelector("h1.breadcrumb-current")!.textContent).toBe("Notes");
});

test("the arch's limit sets the page and the pager moves through the pages", async () => {
  options.root = { page_limit: 20 };
  const { root, of } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(20));
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("21-40 / 45"));
  expect(of("web_search_read").at(-1)!.body).toMatchObject({ offset: 20, limit: 20 });
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(cards(root)).toHaveLength(5));
  expect(root.querySelector<HTMLButtonElement>(".pager-next")!.disabled).toBe(true);
  root.querySelector<HTMLButtonElement>(".pager-prev")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("21-40 / 45"));
});

/// (partial: a click on a card opens its form; the breadcrumb back shows the same page with focus on that card)
test("a card opens its record; the breadcrumb back returns to the same page, scroll and card", async () => {
  options.root = { page_limit: 20 };
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(20));
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".pager-count")!.textContent).toBe("21-40 / 45"));
  const scroller = document.scrollingElement ?? document.documentElement;
  scroller.scrollTop = 120;
  cards(root).find(card => card.dataset["id"] === "23")!.click();
  await vi.waitFor(() => expect(query().get("view_type")).toBe("form"));
  expect(query().get("id")).toBe("23");
  const crumb = [...root.querySelectorAll<HTMLElement>("[data-control-breadcrumbs] a, [data-control-breadcrumbs] button")].find(item => item.textContent === "Notes")!;
  crumb.click();
  await vi.waitFor(() => expect(cards(root)).toHaveLength(20));
  expect(query().get("view_type")).toBe("kanban");
  expect(root.querySelector(".pager-count")!.textContent).toBe("21-40 / 45");
  expect((document.activeElement as HTMLElement).dataset["id"]).toBe("23");
  expect(scroller.scrollTop).toBe(120);
});

test("the form opened from a card steps to the next card of the page", async () => {
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(40));
  cards(root)[0]!.click();
  await vi.waitFor(() => expect(query().get("id")).toBe("1"));
  root.querySelector<HTMLButtonElement>(".form-pager .pager-next, .form-pager [aria-label='Next record']")!.click();
  await vi.waitFor(() => expect(query().get("id")).toBe("2"));
});

test("the search narrows the cards and writes the address; no match shows the empty state", async () => {
  const { root, of } = await startApp("/erp/action-20?view_type=kanban&search=ada");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(1));
  expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["name", "ilike", "ada"]]);
  options.none = true;
  const remove = root.querySelector<HTMLButtonElement>(".o_facet_remove, [aria-label^='Remove']")!;
  remove.click();
  await vi.waitFor(() => expect(root.querySelector(".message-no-content")).not.toBeNull());
  expect(root.querySelector(".message-no-content h2")!.textContent).toBe("No records yet");
  expect(query().has("search_facets")).toBe(false);
});

test("a failed read says so in place, with the pager disabled", async () => {
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(40));
  options.fail = true;
  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".kanban-results .list-load-error")).not.toBeNull());
  expect(root.querySelector(".list-load-error")!.getAttribute("role")).toBe("alert");
  expect(cards(root)).toHaveLength(0);
  expect(root.querySelector(".pager-count")!.textContent).toBe("Results unavailable");
  expect(root.querySelector<HTMLButtonElement>(".pager-next")!.disabled).toBe(true);
});

test("a dashboard card kept in the address narrows the kanban too", async () => {
  options.dashboard = true;
  const { root, of } = await startApp(`/erp/action-20?view_type=kanban&search_filters=${encodeURIComponent(JSON.stringify(["dashboard_done"]))}`);
  await vi.waitFor(() => expect(cards(root)).toHaveLength(15));
  expect(of("erp_read_dashboard")).toHaveLength(1);
  expect(of("web_search_read").at(-1)!.body["domain"]).toEqual([["state", "=", "done"]]);
  expect([...root.querySelectorAll(".search-facet-chip")].map(item => item.textContent)).toEqual(expect.arrayContaining([expect.stringContaining("Done notes")]));
});

test("cards that cannot open are not links, and a view that cannot create shows no New", async () => {
  options.root = { can_open: false, create: false };
  const { root } = await startApp("/erp/action-20?view_type=kanban");
  await vi.waitFor(() => expect(cards(root)).toHaveLength(40));
  expect(root.querySelector(".kanban-notice")).toBeNull(); // the grouped board is client-kanban-columns.test.ts
  expect(root.querySelector(".kanban-card-open")).toBeNull();
  expect([...root.querySelectorAll("[data-control-actions] button")].map(button => button.textContent)).not.toContain("New");
});

test("a switch from a list whose grouping state names no field leaves no grouping in the kanban's address", async () => {
  const { root } = await startApp(`/erp/action-20?search_groupby=${encodeURIComponent(JSON.stringify({ v: 1, by: [], offset: 0, order: "", open: [] }))}`);
  await vi.waitFor(() => expect(root.querySelector(".list-table")).not.toBeNull());
  root.querySelector<HTMLButtonElement>('.view-btn[aria-label="kanban view"]')!.click();
  await vi.waitFor(() => expect(cards(root).length).toBeGreaterThan(0));
  expect(query().has("search_groupby")).toBe(false);
});

test("the served restricted plan renders the card for each permitted record and retains keyboard opening", () => {
  const source = kanbanDescription();
  const digest = "b".repeat(64), arch = "t.note.kanban.7", language = "en_US";
  const plan = { abi: "erpforall.card.v1", policy: "1", arch, language, source_sha256: digest, fields: ["name", "state"],
    templates: { card: { tag: "strong", attrs: { class: "test-card" }, children: [
      { tag: "t", field: ["name", "value"] }, { text: " · " }, { tag: "t", field: ["state", "raw_value"] },
    ] } } };
  const restricted = { arch, language, source_sha256: digest, plan: JSON.stringify(plan) };
  Object.assign(source.views[0]!.nodes[1]!, { restricted });
  const describe = (): View => describeViews(source, "t.note", "r1").views.get("kanban")!;
  const open = vi.fn();
  const grid = renderKanbanCards({ layout: kanbanLayout(describe()), records: rows, open, renderField: plain });
  document.body.append(grid.node);
  expect([...grid.node.querySelectorAll(".test-card")].map(e => e.textContent)).toEqual(rows.map(r => r.name + " · " + r.state));
  const first = grid.node.querySelector<HTMLElement>(".kanban-card")!;
  first.focus();
  first.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  expect(open).toHaveBeenCalledWith(1);
  grid.dispose(); grid.node.remove();
  restricted.source_sha256 = "c".repeat(64);
  expect(() => kanbanLayout(describe())).toThrow(/identity or compiler policy/);
});

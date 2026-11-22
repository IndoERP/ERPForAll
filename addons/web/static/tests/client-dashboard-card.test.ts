/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The dashboard card widget: the contract v1 parser and its refusals, the drawing of
// buttons, figures and both chart types, the action each control opens, keyboard use on a kanban card, and the path
// through the application to the action named by its XML id.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";
import { admitRequestContext, axisLabel, dashboardCardWidget, parseDashboardCard } from "../src/client/dashboard-card.js";
import { CurrencyDisplayCache } from "../src/client/field-currency.js";
import { kanbanLayout, renderKanbanCards } from "../src/client/kanban.js";
import { describeViews, type Field } from "../src/client/metadata.js";
import { configureClientTemplates, fieldWidgets, resolveFieldWidget, type ActionRequest, type WidgetEnvironment } from "../src/client/registry.js";
import { RpcClient } from "../src/client/transport.js";
import "../src/client/fields.js";
import { auditComponent } from "../../../../web/testing/axe.js";

const american = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
const contract = () => ({
  v: 1,
  buttons: [
    { label: "Upload", action: "account.action_move_in_invoice_type", view_mode: null, context: {}, primary: false },
    { label: "New Invoice", action: "account.action_move_out_invoice_type", view_mode: "form", context: { default_move_type: "out_invoice", default_journal_id: 5 }, primary: true }
  ],
  stats: [
    { label: "Late", count: 3, amount: "1250000.00", currency_id: 12, action: "account.action_move_out_invoice_type",
      domain: [["journal_id", "=", 5], ["state", "=", "posted"]], context: { default_journal_id: 5 }, tone: "danger" },
    { label: "To check", count: 1200, amount: null, currency_id: null, action: "account.action_move_journal_line", domain: [], context: {}, tone: "neutral" }
  ],
  chart: { type: "bar", label: "Due", points: [{ label: "Overdue", value: "900000.00", tone: "danger" }, { label: "This week", value: "1250.50", tone: "neutral" }, { label: "Later", value: "-20", tone: "warning" }] }
});
const json = (change?: (value: ReturnType<typeof contract> & Record<string, unknown>) => void): string => {
  const value = contract() as ReturnType<typeof contract> & Record<string, unknown>;
  change?.(value);
  return JSON.stringify(value);
};
const currencyRpc = () => new RpcClient(async () => reply([{ id: 12, symbol: "Rp", position: "before", decimal_places: 2 }]), () => {});
function environment(value: unknown, extra: Partial<WidgetEnvironment> = {}) {
  const opened: ActionRequest[] = [], reported: unknown[] = [];
  const env: WidgetEnvironment = {
    field: { name: "dashboard_card", kind: "text", label: "Dashboard", required: false, readonly: true } as Field, value, change: () => undefined,
    rpc: currencyRpc(), context: {}, run: job => { void job(); }, report: error => { reported.push(error); }, numericLocale: american,
    currencies: new CurrencyDisplayCache(), viewContext: "card", openAction: async request => { opened.push(request); }, ...extra
  };
  return { env, opened, reported };
}

afterEach(() => { document.body.replaceChildren(); });

// ---- the parser ----

test("contract v1 is admitted into a validated card", () => {
  expect(parseDashboardCard(json())).toEqual({
    buttons: [
      { label: "Upload", action: "account.action_move_in_invoice_type", viewMode: null, context: {}, primary: false },
      { label: "New Invoice", action: "account.action_move_out_invoice_type", viewMode: "form", context: { default_move_type: "out_invoice", default_journal_id: 5 }, primary: true }
    ],
    stats: [
      { label: "Late", count: 3, amount: "1250000.00", currencyId: 12, action: "account.action_move_out_invoice_type",
        domain: [["journal_id", "=", 5], ["state", "=", "posted"]], context: { default_journal_id: 5 }, tone: "danger" },
      { label: "To check", count: 1200, amount: null, currencyId: null, action: "account.action_move_journal_line", domain: [], context: {}, tone: "neutral" }
    ],
    chart: { type: "bar", label: "Due", points: [{ label: "Overdue", value: "900000.00", tone: "danger" }, { label: "This week", value: "1250.50", tone: "neutral" }, { label: "Later", value: "-20", tone: "warning" }] }
  });
  expect(parseDashboardCard(json(value => { value.chart = null as never; }))?.chart).toBeNull();
});

test("a record without dashboard data draws an empty card body, not an error", () => {
  for (const value of [null, undefined, false, ""]) {
    expect(parseDashboardCard(value)).toBeNull();
    const { node } = dashboardCardWidget(environment(value).env);
    expect(node.className).toBe("dashboard-card dashboard-card-empty");
    expect(node.childElementCount).toBe(0);
  }
});

test("each departure from contract v1 is refused by the key it concerns and drawn on the card", () => {
  type Card = ReturnType<typeof contract> & Record<string, unknown>;
  const cases: [string | unknown, string][] = [
    [json(value => { value.v = 2; }), "Dashboard card: v 2 is not a known version (expected 1)."],
    [json(value => { delete (value as Partial<Card>).v; }), "Dashboard card: v null is not a known version (expected 1)."],
    [json(value => { delete (value as Partial<Card>).buttons; }), "Dashboard card: card.buttons is missing."],
    [json(value => { value["title"] = "x"; }), "Dashboard card: card.title is not part of contract v1."],
    [json(value => { delete (value.stats[0] as Partial<Card["stats"][0]>).tone; }), "Dashboard card: stats[0].tone is missing."],
    [json(value => { (value.stats[0] as Record<string, unknown>)["count"] = "3"; }), "Dashboard card: stats[0].count must be a whole number of zero or more."],
    [json(value => { (value.stats[0] as Record<string, unknown>)["count"] = -1; }), "Dashboard card: stats[0].count must be a whole number of zero or more."],
    [json(value => { value.stats[0]!.tone = "info"; }), "Dashboard card: stats[0].tone must be neutral, warning or danger."],
    [json(value => { value.chart.points[1]!.tone = "success"; }), "Dashboard card: chart.points[1].tone must be neutral, warning or danger."],
    [json(value => { value.chart.type = "pie"; }), "Dashboard card: chart.type must be \"bar\" or \"line\"."],
    [json(value => { (value.stats[0] as Record<string, unknown>)["amount"] = 1250; }), "Dashboard card: stats[0].amount must be a decimal text such as \"1250.00\"."],
    [json(value => { value.stats[0]!.amount = "1e5"; }), "Dashboard card: stats[0].amount must be a decimal text such as \"1250.00\"."],
    [json(value => { value.chart.points[0]!.value = "nine"; }), "Dashboard card: chart.points[0].value must be a decimal text such as \"1250.00\"."],
    [json(value => { value.stats[0]!.amount = null as never; }), "Dashboard card: stats[0].currency_id must be null exactly when amount is null."],
    [json(value => { (value.buttons[0] as Record<string, unknown>)["view_mode"] = "kanban"; }), "Dashboard card: buttons[0].view_mode must be \"form\", \"list\" or null."],
    [json(value => { (value.buttons[0] as Record<string, unknown>)["primary"] = "yes"; }), "Dashboard card: buttons[0].primary must be true or false."],
    [json(value => { value.buttons[1]!.action = "action_move_out_invoice_type"; }), "Dashboard card: buttons[1].action must be a window action XML id (module.name)."],
    [json(value => { value.buttons[1]!.label = " "; }), "Dashboard card: buttons[1].label must be a non-empty text."],
    [json(value => { (value.stats[0] as Record<string, unknown>)["domain"] = [[1, 2]]; }), "Dashboard card: stats[0].domain[0] must be an operator or a [field, operator, value] condition."],
    [json(value => { (value.stats[0] as Record<string, unknown>)["context"] = []; }), "Dashboard card: stats[0].context must be an object."],
    [json(value => { (value as Record<string, unknown>)["stats"] = {}; }), "Dashboard card: stats must be a list."],
    ["{not json", "Dashboard card: the value is not valid JSON."],
    [{ v: 1 }, "Dashboard card: the value must be JSON text."]
  ];
  for (const [value, message] of cases) {
    expect(() => parseDashboardCard(value), message).toThrowError(expect.objectContaining({ kind: "server", message }));
    const { node } = dashboardCardWidget(environment(value).env);
    expect(node.className).toBe("dashboard-card-error");
    expect(node.textContent).toBe(message);
  }
});

test("a button's or figure's context admits only default and search default keys with plain values", () => {
  const withContext = (context: unknown, stat = false) => json(value => { ((stat ? value.stats[0] : value.buttons[1]) as Record<string, unknown>)["context"] = context; });
  const keys = "is not admitted: only default_<field> and search_default_<name> keys are.";
  const plain = "must be a text, a boolean or a whole number.";
  const cases: [string, string][] = [
    [withContext({ allowed_company_ids: [1, 2] }), `Dashboard card: buttons[1].context["allowed_company_ids"] ${keys}`],
    [withContext({ default_move_type: "out_invoice", lang: "fr_FR" }), `Dashboard card: buttons[1].context["lang"] ${keys}`],
    [withContext({ tz: "UTC" }), `Dashboard card: buttons[1].context["tz"] ${keys}`],
    [withContext({ active_test: false }), `Dashboard card: buttons[1].context["active_test"] ${keys}`],
    [withContext({ bin_size: true }), `Dashboard card: buttons[1].context["bin_size"] ${keys}`],
    [withContext({ journal_type: "sale" }), `Dashboard card: buttons[1].context["journal_type"] ${keys}`],
    [withContext({ default_: 1 }), `Dashboard card: buttons[1].context["default_"] ${keys}`],
    [withContext({ Default_Move_Type: "entry" }), `Dashboard card: buttons[1].context["Default_Move_Type"] ${keys}`],
    [withContext({ default_journal_id: { id: 5 } }), `Dashboard card: buttons[1].context["default_journal_id"] ${plain}`],
    [withContext({ default_journal_id: [5] }), `Dashboard card: buttons[1].context["default_journal_id"] ${plain}`],
    [withContext({ default_journal_id: 5.5 }), `Dashboard card: buttons[1].context["default_journal_id"] ${plain}`],
    [withContext({ default_journal_id: null }), `Dashboard card: buttons[1].context["default_journal_id"] ${plain}`],
    // A search default names a search item: a plain identifier with a plain value.
    [withContext({ "search_default_my-filter": 1 }), `Dashboard card: buttons[1].context["search_default_my-filter"] ${keys}`],
    [withContext({ search_default_: 1 }), `Dashboard card: buttons[1].context["search_default_"] ${keys}`],
    [withContext({ search_default_journal_id: [5] }, true), `Dashboard card: stats[0].context["search_default_journal_id"] ${plain}`],
    [withContext(Object.fromEntries(Array.from({ length: 17 }, (_, index) => [`default_field_${index}`, index]))), "Dashboard card: buttons[1].context has more than 16 keys."],
    [withContext({ lang: "fr_FR" }, true), `Dashboard card: stats[0].context["lang"] ${keys}`],
    [withContext({ default_journal_id: 5.5 }, true), `Dashboard card: stats[0].context["default_journal_id"] ${plain}`]
  ];
  for (const [value, message] of cases) {
    expect(() => parseDashboardCard(value), message).toThrowError(expect.objectContaining({ kind: "server", message }));
    const { node } = dashboardCardWidget(environment(value).env);
    expect(node.className).toBe("dashboard-card-error");
    expect(node.textContent).toBe(message);
  }
  // The keys the server sends today, and sixteen keys, pass unchanged.
  const real = { default_move_type: "out_invoice", default_journal_id: 5, default_picking_type_id: 3 };
  expect(parseDashboardCard(withContext(real))!.buttons[1]!.context).toEqual(real);
  expect(parseDashboardCard(withContext(real, true))!.stats[0]!.context).toEqual(real);
  // A figure may open its list with a filter preselected that the user can still remove.
  const preselected = { search_default_journal_id: 5, search_default_posted: true, default_move_type: "entry" };
  expect(parseDashboardCard(withContext(preselected, true))!.stats[0]!.context).toEqual(preselected);
  const sixteen = Object.fromEntries(Array.from({ length: 16 }, (_, index) => [`default_field_${index}`, index % 2 === 0 ? index : `value ${index}`]));
  expect(parseDashboardCard(withContext({ ...sixteen, default_field_0: true }))!.buttons[1]!.context).toEqual({ ...sixteen, default_field_0: true });
});

test("the application checks a request's context again where it uses it", () => {
  expect(admitRequestContext({ default_move_type: "out_invoice", default_journal_id: 5 }, "the request context")).toEqual({ default_move_type: "out_invoice", default_journal_id: 5 });
  expect(admitRequestContext({ search_default_posted: 1 }, "the request context")).toEqual({ search_default_posted: 1 });
  for (const [value, message] of [
    [{ allowed_company_ids: [9] }, "Dashboard card: the request context[\"allowed_company_ids\"] is not admitted: only default_<field> and search_default_<name> keys are."],
    [{ "search_default_a.b": 5 }, "Dashboard card: the request context[\"search_default_a.b\"] is not admitted: only default_<field> and search_default_<name> keys are."],
    [{ default_journal_id: [5] }, "Dashboard card: the request context[\"default_journal_id\"] must be a text, a boolean or a whole number."],
    [[], "Dashboard card: the request context must be an object."]
  ] as [unknown, string][]) {
    expect(() => admitRequestContext(value, "the request context"), message).toThrowError(expect.objectContaining({ kind: "server", message }));
  }
});

// ---- drawing ----

test("buttons come primary first as house buttons; figures show count, label, amount in its currency and tone", async () => {
  const { env } = environment(json());
  const { node } = dashboardCardWidget(env);
  document.body.append(node);
  const buttons = [...node.querySelectorAll<HTMLButtonElement>(".dashboard-card-buttons button")];
  expect(buttons.map(button => button.textContent)).toEqual(["New Invoice", "Upload"]);
  expect(buttons.map(button => button.className)).toEqual(["erp-button erp-button-primary erp-button-small", "erp-button erp-button-secondary erp-button-small"]);
  const stats = [...node.querySelectorAll<HTMLButtonElement>(".dashboard-card-stat")];
  expect(stats.map(stat => stat.classList.contains("dashboard-card-tone-danger"))).toEqual([true, false]);
  expect(stats[1]!.textContent).toBe("1,200To check");
  await vi.waitFor(() => expect(stats[0]!.querySelector(".dashboard-card-stat-amount")!.getAttribute("aria-busy")).toBeNull());
  expect(stats[0]!.textContent).toBe("3LateRp1,250,000.00");
  expect(stats[0]!.querySelector(".field-currency-symbol")!.textContent).toBe("Rp");
  await auditComponent(node); // throws on a serious or critical finding
});

test("an unreadable currency leaves the figure readable and reports why", async () => {
  const refused = new RpcClient(async () => reply({ name: "erp.exceptions.AccessError", message: "You may not read currencies." }, 403), () => {});
  const { env, reported } = environment(json(), { rpc: refused });
  const { node } = dashboardCardWidget(env);
  await vi.waitFor(() => expect(reported).toHaveLength(1));
  expect(node.querySelector(".dashboard-card-stat-amount")!.textContent).toBe("Amount unavailable");
  expect(node.querySelector(".dashboard-card-stat")!.textContent).toContain("3Late");
});

test("the bar chart draws one bar per point in its tone, with the points as text for assistive technology", () => {
  const { node } = dashboardCardWidget(environment(json()).env);
  const figure = node.querySelector("figure.dashboard-card-chart")!;
  expect(figure.querySelector("figcaption")!.textContent).toBe("Due");
  const drawing = figure.querySelector("svg")!;
  expect(drawing.getAttribute("aria-hidden")).toBe("true");
  expect([...drawing.querySelectorAll("rect")].map(bar => bar.getAttribute("class"))).toEqual(["graph-bar dashboard-card-tone-danger", "graph-bar dashboard-card-tone-neutral", "graph-bar dashboard-card-tone-warning"]);
  expect(drawing.querySelector("polyline")).toBeNull();
  expect([...figure.querySelectorAll(".dashboard-card-chart-axis span")].map(item => item.textContent)).toEqual(["Overdue", "This week", "Later"]);
  expect([...figure.querySelectorAll("ul.visually-hidden li")].map(item => item.textContent)).toEqual(["Overdue: 900,000 (danger)", "This week: 1,250.5", "Later: -20 (warning)"]);
});

test("a long chart labels at most six points, counted from the last, as short dates", () => {
  const days = Array.from({ length: 30 }, (_, index) => new Date(Date.UTC(2026, 9, 19 + index)).toISOString().slice(0, 10));
  const { node } = dashboardCardWidget(environment(json(value => {
    value.chart.type = "line";
    value.chart.points = days.map(label => ({ label, value: "0", tone: "neutral" }));
  })).env);
  const ticks = [...node.querySelectorAll<HTMLElement>(".dashboard-card-chart-axis span")];
  expect(ticks.map(item => item.textContent)).toEqual(["Oct 23", "Oct 28", "Nov 2", "Nov 7", "Nov 12", "Nov 17"]);
  expect(ticks.map(item => item.style.gridColumn)).toEqual(["5", "10", "15", "20", "25", "30"]);
  expect(ticks.at(-1)!.style.justifySelf).toBe("end");
  // every point stays in the text alternative, with its full label
  expect(node.querySelectorAll("figure ul.visually-hidden li")).toHaveLength(30);
  expect(node.querySelector("figure ul.visually-hidden li")!.textContent).toBe("2026-10-19: 0");
});

test("axis labels read ISO days and months as short dates and leave other labels alone", () => {
  expect(axisLabel("2026-11-17")).toBe("Nov 17");
  expect(axisLabel("2025-11")).toBe("Nov 25");
  expect(axisLabel("Overdue")).toBe("Overdue");
  expect(axisLabel("2026-13")).toBe("2026-13");
});

test("the day label orders day and month by the page language", () => {
  const lang = document.documentElement.lang;
  try {
    document.documentElement.lang = "en-GB";
    expect(axisLabel("2026-11-17")).toMatch(/^17 Nov/);
    document.documentElement.lang = "en-US";
    expect(axisLabel("2026-11-17")).toMatch(/^Nov 17/);
  } finally { document.documentElement.lang = lang; }
});

test("the line chart draws one line through every point, with the same text alternative", () => {
  const { node } = dashboardCardWidget(environment(json(value => { value.chart.type = "line"; })).env);
  const drawing = node.querySelector("figure.dashboard-card-chart svg")!;
  expect(drawing.getAttribute("class")).toBe("dashboard-card-chart-line");
  expect(drawing.querySelectorAll("rect")).toHaveLength(0);
  expect(drawing.querySelector("polyline")!.getAttribute("points")!.split(" ")).toHaveLength(3);
  expect(node.querySelectorAll("figure ul.visually-hidden li")).toHaveLength(3);
});

test("a chart without points says so", () => {
  const { node } = dashboardCardWidget(environment(json(value => { value.chart.points = []; })).env);
  expect(node.querySelector(".dashboard-card-chart-empty")!.textContent).toBe("No figures for this period.");
  expect(node.querySelector("figure svg")).toBeNull();
});

// ---- behaviour ----

test("a button opens its action with its context; a figure opens its action's list with its exact domain and context", () => {
  const { env, opened } = environment(json());
  const { node } = dashboardCardWidget(env);
  node.querySelector<HTMLButtonElement>(".dashboard-card-buttons .erp-button-primary")!.click();
  node.querySelector<HTMLButtonElement>(".dashboard-card-buttons .erp-button-secondary")!.click();
  node.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  expect(opened).toEqual([
    { xmlId: "account.action_move_out_invoice_type", viewMode: "form", domain: [], context: { default_move_type: "out_invoice", default_journal_id: 5 } },
    { xmlId: "account.action_move_in_invoice_type", viewMode: null, domain: [], context: {} },
    { xmlId: "account.action_move_out_invoice_type", viewMode: "list", domain: [["journal_id", "=", 5], ["state", "=", "posted"]], context: { default_journal_id: 5 } }
  ]);
});

test("on a kanban card the controls are keyboard reachable buttons, and using one does not also open the card's record", async () => {
  const { env, opened } = environment(json());
  const description = describeViews({
    wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.journal", notices: [],
    fields: { "t.journal": { name: { type: "char", label: "Name", required: false, readonly: false, relation: null, relation_field: null, selection: null },
      dashboard: { type: "text", label: "Dashboard", required: false, readonly: true, relation: null, relation_field: null, selection: null } } },
    views: [{ identity: { kind: "stored", id: 7 }, type: "kanban", schema_major: 1, renderer: "web.kanban", root: 0, nodes: [
      { kind: "kanban", children: [1], quick_create: true, records_draggable: true, groups_draggable: true, archivable: true, can_open: true },
      { kind: "kanban_card", children: [2, 3] },
      { kind: "field", force_save: false, field: { model: "t.journal", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" },
      { kind: "field", force_save: false, field: { model: "t.journal", name: "dashboard" }, widget: "dashboard_card", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Dashboard" }
    ] }]
  }, "t.journal", "r1").views.get("kanban")!;
  const records = [{ id: 5, name: "Customer Invoices", dashboard: json() }];
  const cardOpened: number[] = [];
  const grid = renderKanbanCards({ layout: kanbanLayout(description), records, open: id => cardOpened.push(id), renderField: (field, record) => {
    if (field.widget !== "dashboard_card") { const node = document.createElement("span"); node.textContent = String(record[field.name]); return { node, dispose: () => undefined }; }
    return resolveFieldWidget(field.widget, "card").render({ ...env, field, value: record[field.name], values: record });
  } });
  document.body.append(grid.node);
  const card = grid.node.querySelector<HTMLElement>(".kanban-card")!;
  const controls = [...card.querySelectorAll<HTMLButtonElement>(".dashboard-card button")];
  expect(controls).toHaveLength(4); // two buttons, two figures
  for (const control of controls) {
    expect(control.type).toBe("button");
    expect(control.tabIndex).toBe(0);
    expect(control.disabled).toBe(false);
  }
  const stat = card.querySelector<HTMLButtonElement>(".dashboard-card-stat")!;
  stat.focus();
  expect(document.activeElement).toBe(stat);
  // Enter on a control belongs to the control: the card's own Enter (open the record) does not fire.
  stat.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  expect(cardOpened).toEqual([]);
  stat.click(); // what the browser does for Enter or Space on a native button
  expect(opened.map(request => request.viewMode)).toEqual(["list"]);
  expect(cardOpened).toEqual([]);
  card.querySelector<HTMLButtonElement>(".dashboard-card-buttons .erp-button-primary")!.click();
  expect(opened.map(request => request.viewMode)).toEqual(["list", "form"]);
  expect(cardOpened).toEqual([]);
  // The card itself still opens its record.
  card.querySelector<HTMLElement>(".kanban-card-title")!.click();
  expect(cardOpened).toEqual([5]);
  await auditComponent(grid.node);
  grid.dispose();
});

test("the widget name is admitted for text fields; outside a kanban card it says where it belongs", () => {
  expect(fieldWidgets.get("card.dashboard_card").kinds).toEqual(["text"]);
  expect(resolveFieldWidget("dashboard_card", "card")).toBe(fieldWidgets.get("card.dashboard_card"));
  const { node } = resolveFieldWidget("dashboard_card", "form").render(environment(json()).env);
  expect(node.textContent).toBe("The dashboard card widget is drawn on kanban cards only.");
});

test("without an action opener the card shows why instead of drawing dead controls", () => {
  const { node } = dashboardCardWidget(environment(json(), { openAction: undefined }).env);
  expect(node.textContent).toBe("The dashboard card opens actions only on a kanban card.");
});

// ---- through the application ----
test("one failed currency read is reported once for every figure that waited on it; a disposed card reports and draws nothing", async () => {
  const refused = () => new RpcClient(async () => reply({ name: "erp.exceptions.AccessError", message: "You may not read currencies." }, 403), () => {});
  const amounts = (node: HTMLElement) => [...node.querySelectorAll(".dashboard-card-stat-amount")].map(item => item.textContent);
  // two money figures per card, three cards of one view (one currency cache)
  const twoAmounts = json(value => { value.stats[1] = { ...value.stats[0]!, label: "Unpaid" } as never; });
  const shared = { rpc: refused(), currencies: new CurrencyDisplayCache() }, reported: unknown[] = [];
  const make = () => dashboardCardWidget(environment(twoAmounts, { ...shared, report: error => { reported.push(error); } }).env);
  const kept = make(), other = make(), replaced = make();
  replaced.dispose();
  await vi.waitFor(() => expect(amounts(other.node)).toEqual(["Amount unavailable", "Amount unavailable"]));
  expect(amounts(kept.node)).toEqual(["Amount unavailable", "Amount unavailable"]);
  expect(amounts(replaced.node)).toEqual(["…", "…"]);
  expect(reported).toHaveLength(1);
  // a card that was replaced before the answer is the only one waiting: nothing is reported
  const alone = environment(json(), { rpc: refused() });
  dashboardCardWidget(alone.env).dispose();
  await new Promise(resolve => setTimeout(resolve, 20));
  expect(alone.reported).toEqual([]);
});

const column = (type: string, label: string) => ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null });
const modifiers = { readonly: null, required: null, invisible: null };
const field = (model: string, name: string, widget: string, label: string) => ({ kind: "field", force_save: false, field: { model, name }, widget, options: {}, modifiers, label });
const search = (id: number) => ({ identity: { kind: "stored", id }, type: "search", schema_major: 1, renderer: "web.search", root: 0, nodes: [{ kind: "search", children: [] }] });
const journals = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.journal", notices: [],
  fields: { "t.journal": { name: column("char", "Name"), dashboard: column("text", "Dashboard") } },
  views: [{ identity: { kind: "stored", id: 7 }, type: "kanban", schema_major: 1, renderer: "web.kanban", root: 0, nodes: [
    { kind: "kanban", children: [1], quick_create: true, records_draggable: true, groups_draggable: true, archivable: true, can_open: true },
    { kind: "kanban_card", children: [2, 3] }, field("t.journal", "name", "char", "Name"), field("t.journal", "dashboard", "dashboard_card", "Dashboard")
  ] }, search(4)]
};
const moves = {
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.move", notices: [],
  fields: { "t.move": { name: column("char", "Number") } },
  views: [
    { identity: { kind: "stored", id: 12 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, field("t.move", "name", "char", "Number")] },
    { identity: { kind: "stored", id: 13 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, field("t.move", "name", "char", "Number")] },
    search(14)
  ]
};
const bootstrap = {
  wire_major: 1, registry_version: "r1", asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S", ...american },
  csrf: { logout: "test-token-not-secret" }
};
let app: HumanApp | undefined;
beforeEach(() => {
  configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const node = document.createElement("div"); node.setAttribute(`data-control-${key}`, ""); root.append(node); }
    return root;
  } }]);
});
afterEach(() => { app?.dispose(); app = undefined; vi.restoreAllMocks(); vi.unstubAllGlobals(); history.replaceState(null, "", "/"); });

test("through the application a figure loads its action by XML id and opens its list with the action's domain, the figure's domain and context", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const calls: { model: string; method: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const parts = url.split("/"), method = parts.at(-1)!, model = parts.at(-2)!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ model, method, body });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Journals", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Journals", model: "t.journal", target: "current", views: [{ id: 7, type: "kanban" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: journals } : action);
    }
    if (method === "erp_load_web_action_ref") {
      // The action's own domain, as the server sends an action domain: an expression the client evaluates.
      const domain = { schema: 1, nodes: [{ kind: "literal", value: { kind: "str", value: "move_type" } }, { kind: "literal", value: { kind: "str", value: "=" } },
        { kind: "literal", value: { kind: "str", value: "out_invoice" } }, { kind: "tuple", items: [0, 1, 2] }, { kind: "list", items: [3] }] };
      return reply({ wire_major: 1, id: 30, name: "Invoices", model: "t.move", target: "current", views: [{ id: 12, type: "list" }, { id: 13, type: "form" }], domain, context: null });
    }
    if (method === "get_views") return reply(model === "t.move" ? moves : journals);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true } });
    if (method === "web_search_read") return reply(model === "t.move" ? { length: 0, records: [] } : { length: 1, records: [{ id: 5, name: "Customer Invoices", dashboard: json() }] });
    if (model === "res.currency" && method === "web_read") return reply([{ id: 12, symbol: "Rp", position: "before", decimal_places: 2 }]);
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  await vi.waitFor(() => expect(root.querySelector(".dashboard-card-stat")).not.toBeNull());
  root.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  await vi.waitFor(() => expect(calls.some(call => call.model === "t.move" && call.method === "web_search_read")).toBe(true));
  const reference = calls.find(call => call.method === "erp_load_web_action_ref")!;
  expect(reference.model).toBe("ir.actions.act_window");
  expect(reference.body).toMatchObject({ xml_id: "account.action_move_out_invoice_type" });
  const read = calls.filter(call => call.model === "t.move" && call.method === "web_search_read").at(-1)!.body;
  expect(read["domain"]).toEqual([["move_type", "=", "out_invoice"], ["journal_id", "=", 5], ["state", "=", "posted"]]);
  expect(read["context"]).toMatchObject({ default_journal_id: 5 });
  expect(new URL(window.location.href).searchParams.get("view_type")).toBe("list");
});

test("the action's own literal defaults travel with a figure's list, so New there creates with them", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const calls: { model: string; method: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
    const parts = url.split("/"), method = parts.at(-1)!, model = parts.at(-2)!, body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    calls.push({ model, method, body });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Journals", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Journals", model: "t.journal", target: "current", views: [{ id: 7, type: "kanban" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: journals } : action);
    }
    if (method === "erp_load_web_action_ref") {
      // `{'default_move_type': 'out_invoice'}` as the server sends an admitted action context.
      const context = { schema: 1, nodes: [{ kind: "literal", value: { kind: "str", value: "default_move_type" } },
        { kind: "literal", value: { kind: "str", value: "out_invoice" } }, { kind: "dict", items: [[0, 1]] }] };
      return reply({ wire_major: 1, id: 30, name: "Invoices", model: "t.move", target: "current", views: [{ id: 12, type: "list" }, { id: 13, type: "form" }], domain: null, context });
    }
    if (method === "get_views") return reply(model === "t.move" ? moves : journals);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true } });
    if (method === "web_search_read") return reply(model === "t.move" ? { length: 0, records: [] } : { length: 1, records: [{ id: 5, name: "Customer Invoices", dashboard: json() }] });
    if (method === "default_get") return reply({ name: "" });
    if (method === "onchange") return reply({ value: {} });
    if (model === "res.currency" && method === "web_read") return reply([{ id: 12, symbol: "Rp", position: "before", decimal_places: 2 }]);
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  await vi.waitFor(() => expect(root.querySelector(".dashboard-card-stat")).not.toBeNull());
  root.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  await vi.waitFor(() => expect(calls.some(call => call.model === "t.move" && call.method === "web_search_read")).toBe(true));
  const read = calls.filter(call => call.model === "t.move" && call.method === "web_search_read").at(-1)!.body;
  expect(read["context"]).toMatchObject({ default_move_type: "out_invoice", default_journal_id: 5 });
  // The session's own keys are handed on as the session set them, next to the admitted defaults.
  expect(read["context"]).toMatchObject({ allowed_company_ids: [1], lang: "en_US", tz: "UTC" });
  const create = [...root.querySelectorAll<HTMLButtonElement>("[data-control-actions] button")].find(node => node.textContent?.trim() === "New");
  expect(create).toBeDefined();
  create!.click();
  await vi.waitFor(() => expect(calls.some(call => call.model === "t.move" && call.method === "default_get")).toBe(true));
  const defaults = calls.find(call => call.model === "t.move" && call.method === "default_get")!.body;
  expect(defaults["context"]).toMatchObject({ default_move_type: "out_invoice" });
});

/** The journals kanban with one card, and `actionRef` answering the read of the action a figure names. */
async function journalsPage(actionRef: () => Response | Promise<Response>) {
  history.replaceState(null, "", "/erp/action-20");
  const calls: { model: string; method: string }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string) => {
    const parts = url.split("/"), method = parts.at(-1)!, model = parts.at(-2)!;
    calls.push({ model, method });
    if (method === "erp_load_web_menus") return reply({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Journals", action_id: 20, children: [], icon: null }] });
    if (method === "erp_load_web_action" || method === "erp_load_web_action_views") {
      const action = { wire_major: 1, id: 20, name: "Journals", model: "t.journal", target: "current", views: [{ id: 7, type: "kanban" }], domain: null, context: null };
      return reply(method === "erp_load_web_action_views" ? { action, views: journals } : action);
    }
    if (method === "erp_load_web_action_ref") return actionRef();
    if (method === "get_views") return reply(model === "t.move" ? moves : journals);
    if (method === "fields_get") return reply({ name: { string: "Name", type: "char", groupable: true } });
    if (method === "web_search_read") return reply(model === "t.move" ? { length: 0, records: [] } : { length: 1, records: [{ id: 5, name: "Customer Invoices", dashboard: json() }] });
    if (method === "default_get") return reply({ name: "" });
    if (method === "onchange") return reply({ value: {} });
    if (model === "res.currency" && method === "web_read") return reply([{ id: 12, symbol: "Rp", position: "before", decimal_places: 2 }]);
    return reply([]);
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) { const node = document.createElement("div"); node.setAttribute(`data-client-${region}`, ""); root.append(node); }
  document.body.append(root);
  app = new HumanApp(root, bootstrap);
  await app.start();
  await vi.waitFor(() => expect(root.querySelector(".dashboard-card-stat")).not.toBeNull());
  return { root, calls, asked: () => calls.filter(call => call.method === "erp_load_web_action_ref").length, listed: () => calls.some(call => call.model === "t.move" && call.method === "web_search_read") };
}
const invoices = () => reply({ wire_major: 1, id: 30, name: "Invoices", model: "t.move", target: "current", views: [{ id: 12, type: "list" }, { id: 13, type: "form" }], domain: null, context: null });
const settled = () => new Promise(resolve => setTimeout(resolve, 30));

test("a card button that opens a form names the dashboard once: the back link is the way back to it", async () => {
  const page = await journalsPage(invoices);
  [...page.root.querySelectorAll<HTMLButtonElement>(".kanban-card button")].find(node => node.textContent?.trim() === "New Invoice")!.click();
  await vi.waitFor(() => expect(page.root.querySelector(".breadcrumb-back")).not.toBeNull());
  expect(new URL(window.location.href).searchParams.get("view_type")).toBe("form");
  // The trail is [dashboard, record]: the back link already leads to the origin, so no second crumb names it.
  expect(page.root.querySelector(".breadcrumb-back")!.textContent).toBe("Journals");
  expect(page.root.querySelector(".breadcrumb-origin")).toBeNull();
});

test("a figure activated twice while its action is read opens its list once", async () => {
  const answers: ((response: Response) => void)[] = [];
  const page = await journalsPage(() => new Promise<Response>(resolve => { answers.push(resolve); }));
  const entries = history.length;
  page.root.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  page.root.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  await vi.waitFor(() => expect(page.asked()).toBe(2));
  answers[0]!(invoices());
  await settled();
  // the earlier request is not the latest one: its answer opens nothing
  expect(page.listed()).toBe(false);
  expect(history.length).toBe(entries);
  answers[1]!(invoices());
  await vi.waitFor(() => expect(page.listed()).toBe(true));
  await settled();
  expect(history.length).toBe(entries + 1);
  expect(new URL(window.location.href).searchParams.get("view_type")).toBe("list");
  expect(page.root.querySelector("[data-client-notice]")!.textContent).not.toContain("Wait for the current operation to finish.");
});

test("a figure's action that answers after the user left the page opens nothing", async () => {
  const answers: ((response: Response) => void)[] = [];
  const page = await journalsPage(() => new Promise<Response>(resolve => { answers.push(resolve); }));
  page.root.querySelector<HTMLButtonElement>(".dashboard-card-stat")!.click();
  await vi.waitFor(() => expect(page.asked()).toBe(1));
  // the user goes back to the home page while the read runs: a navigation of their own
  history.replaceState({ erpIndex: -1 }, "", "/erp");
  window.dispatchEvent(new PopStateEvent("popstate", { state: { erpIndex: -1 } }));
  await vi.waitFor(() => expect(page.root.querySelector(".home-grid")).not.toBeNull());
  const entries = history.length, address = window.location.href;
  answers[0]!(invoices());
  await settled();
  expect(page.listed()).toBe(false);
  expect(history.length).toBe(entries);
  expect(window.location.href).toBe(address);
  expect(page.root.querySelector(".home-grid")).not.toBeNull();
});

test("zero chart values draw no bar above or below the baseline", () => {
  const value = json(card => { card.chart.points[0]!.value = "0"; });
  const { node } = dashboardCardWidget(environment(value).env);
  const bars = node.querySelectorAll(".dashboard-card-chart rect");
  expect(bars[0]?.getAttribute("height")).toBe("0");
  expect(Number(bars[1]?.getAttribute("height"))).toBeGreaterThan(0);
});

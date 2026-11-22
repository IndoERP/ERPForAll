/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The `dashboard_card` field widget: a kanban card's buttons, figures and chart,
// read from one JSON text value (contract v1). The parser admits exactly the contract; anything else is an error that
// names the key, drawn on the card. Buttons and figures open window actions through the application.
import { createButtonControl } from "./button-control.js";
import { el } from "./dom.js";
import { validateMonetaryAmount } from "./field-currency.js";
import { svg } from "./graph-view.js";
import { formatNumeric, type NumericLocale } from "./numeric-values.js";
import { registerFieldWidget, type ActionRequest, type WidgetEnvironment, type WidgetHandle } from "./registry.js";
import { ClientError } from "./transport.js";

export type DashboardTone = "neutral" | "warning" | "danger";
export interface DashboardButton { label: string; action: string; viewMode: "form" | "list" | null; context: Record<string, unknown>; primary: boolean }
export interface DashboardStat {
  label: string; count: number; amount: string | null; currencyId: number | null;
  action: string; domain: unknown[]; context: Record<string, unknown>; tone: DashboardTone;
}
export interface DashboardPoint { label: string; value: string; tone: DashboardTone }
export interface DashboardChart { type: "bar" | "line"; label: string; points: DashboardPoint[] }
export interface DashboardCard { buttons: DashboardButton[]; stats: DashboardStat[]; chart: DashboardChart | null }

// Admission bounds of server data drawn on one card (safety limits, not presentation choices).
const MAX_TEXT = 65536, MAX_ITEMS = 64, MAX_LABEL = 256;
const TONES: readonly string[] = ["neutral", "warning", "danger"];
const refuse = (path: string, problem: string): never => { throw new ClientError("server", `Dashboard card: ${path} ${problem}.`); };
type Json = Record<string, unknown>;

function record(value: unknown, path: string, keys: readonly string[]): Json {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return refuse(path, "must be an object");
  const object = value as Json;
  for (const key of keys) if (!Object.hasOwn(object, key)) refuse(`${path}.${key}`, "is missing");
  for (const key of Object.keys(object)) if (!keys.includes(key)) refuse(`${path}.${key}`, "is not part of contract v1");
  return object;
}
function list(value: unknown, path: string): unknown[] {
  if (!Array.isArray(value)) return refuse(path, "must be a list");
  if (value.length > MAX_ITEMS) refuse(path, `has more than ${MAX_ITEMS} entries`);
  return value;
}
function label(value: unknown, path: string): string {
  if (typeof value !== "string" || !value.trim()) return refuse(path, "must be a non-empty text");
  if (value.length > MAX_LABEL) refuse(path, `is longer than ${MAX_LABEL} characters`);
  return value;
}
function reference(value: unknown, path: string): string {
  // `module.name`, as the server's window-action reference admits it.
  if (typeof value !== "string" || value.length > MAX_LABEL || !/^[A-Za-z0-9_]+\.[^.\s]+$/.test(value)) return refuse(path, "must be a window action XML id (module.name)");
  return value;
}
function decimal(value: unknown, path: string): string {
  if (typeof value !== "string" || !/^-?\d{1,30}(\.\d{1,30})?$/.test(value)) return refuse(path, "must be a decimal text such as \"1250.00\"");
  return value;
}
function tone(value: unknown, path: string): DashboardTone {
  if (typeof value !== "string" || !TONES.includes(value)) return refuse(path, "must be neutral, warning or danger");
  return value as DashboardTone;
}
/** Most keys of the context a button or figure hands to its action. */
const MAX_CONTEXT_KEYS = 16;
/** The context a card's button or figure hands to its action: at most 16 `default_<field>` and `search_default_<name>`
 *  keys, each with a text, boolean or safe-integer value. A `search_default_<name>` preselects a filter, group-by item or
 *  field of the action's search view that the user can still remove. Session keys (companies, language, time zone) and
 *  every other key or value form are refused by name. The application checks a request's context again where it uses it. */
export function admitRequestContext(value: unknown, path: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return refuse(path, "must be an object");
  const entries = Object.entries(value as Json);
  if (entries.length > MAX_CONTEXT_KEYS) refuse(path, `has more than ${MAX_CONTEXT_KEYS} keys`);
  const admitted: Record<string, unknown> = {};
  for (const [key, item] of entries) {
    // A key is quoted and clipped: it is server data on its way into a message.
    const at = `${path}[${JSON.stringify(key.slice(0, 64))}]`;
    if (!/^default_[a-z_][a-z0-9_]*$/.test(key) && !/^search_default_[A-Za-z_][A-Za-z0-9_]*$/.test(key)) refuse(at, "is not admitted: only default_<field> and search_default_<name> keys are");
    if (!(typeof item === "string" || typeof item === "boolean" || typeof item === "number" && Number.isSafeInteger(item))) refuse(at, "must be a text, a boolean or a whole number");
    admitted[key] = item;
  }
  return admitted;
}
const context = admitRequestContext;
function domain(value: unknown, path: string): unknown[] {
  const terms = list(value, path);
  terms.forEach((term, index) => {
    if (term === "&" || term === "|" || term === "!") return;
    if (!Array.isArray(term) || term.length !== 3 || typeof term[0] !== "string" || typeof term[1] !== "string") refuse(`${path}[${index}]`, "must be an operator or a [field, operator, value] condition");
  });
  return terms;
}

/** Contract v1 into a validated card. Null, false or an empty text is a record without dashboard data. */
export function parseDashboardCard(raw: unknown): DashboardCard | null {
  if (raw === null || raw === undefined || raw === false || raw === "") return null;
  if (typeof raw !== "string") return refuse("the value", "must be JSON text");
  if (raw.length > MAX_TEXT) refuse("the value", `is longer than ${MAX_TEXT} characters`);
  let parsed: unknown;
  try { parsed = JSON.parse(raw); } catch { return refuse("the value", "is not valid JSON"); }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return refuse("the value", "must be an object");
  if ((parsed as Json)["v"] !== 1) refuse("v", `${JSON.stringify((parsed as Json)["v"] ?? null)} is not a known version (expected 1)`);
  const card = record(parsed, "card", ["v", "buttons", "stats", "chart"]);
  const buttons = list(card["buttons"], "buttons").map((item, index): DashboardButton => {
    const path = `buttons[${index}]`, button = record(item, path, ["label", "action", "view_mode", "context", "primary"]);
    const viewMode = button["view_mode"];
    if (viewMode !== null && viewMode !== "form" && viewMode !== "list") refuse(`${path}.view_mode`, "must be \"form\", \"list\" or null");
    if (typeof button["primary"] !== "boolean") refuse(`${path}.primary`, "must be true or false");
    return { label: label(button["label"], `${path}.label`), action: reference(button["action"], `${path}.action`), viewMode: viewMode as DashboardButton["viewMode"],
      context: context(button["context"], `${path}.context`), primary: button["primary"] as boolean };
  });
  const stats = list(card["stats"], "stats").map((item, index): DashboardStat => {
    const path = `stats[${index}]`, stat = record(item, path, ["label", "count", "amount", "currency_id", "action", "domain", "context", "tone"]);
    const count = stat["count"];
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) refuse(`${path}.count`, "must be a whole number of zero or more");
    const amount = stat["amount"] === null ? null : decimal(stat["amount"], `${path}.amount`);
    const currencyId = stat["currency_id"];
    if (currencyId !== null && (typeof currencyId !== "number" || !Number.isSafeInteger(currencyId) || currencyId <= 0)) refuse(`${path}.currency_id`, "must be a currency id or null");
    if ((amount === null) !== (currencyId === null)) refuse(`${path}.currency_id`, "must be null exactly when amount is null");
    return { label: label(stat["label"], `${path}.label`), count: count as number, amount, currencyId: currencyId as number | null,
      action: reference(stat["action"], `${path}.action`), domain: domain(stat["domain"], `${path}.domain`), context: context(stat["context"], `${path}.context`),
      tone: tone(stat["tone"], `${path}.tone`) };
  });
  let chart: DashboardChart | null = null;
  if (card["chart"] !== null) {
    const body = record(card["chart"], "chart", ["type", "label", "points"]);
    if (body["type"] !== "bar" && body["type"] !== "line") refuse("chart.type", "must be \"bar\" or \"line\"");
    chart = { type: body["type"] as "bar" | "line", label: label(body["label"], "chart.label"), points: list(body["points"], "chart.points").map((item, index): DashboardPoint => {
      const path = `chart.points[${index}]`, point = record(item, path, ["label", "value", "tone"]);
      return { label: label(point["label"], `${path}.label`), value: decimal(point["value"], `${path}.value`), tone: tone(point["tone"], `${path}.tone`) };
    }) };
  }
  return { buttons, stats, chart };
}

const number = (value: string): number => {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || Math.abs(parsed) > Number.MAX_SAFE_INTEGER) throw new ClientError("unsupported", `The figure ${value} is too large to draw.`);
  return parsed;
};

/** At most this many labels under a chart: a card is narrow, and one label per point of a 30-day line cannot be read. */
const AXIS_LABELS = 6;

/** A point label for the axis: an ISO day (`2026-11-17`) reads "17 Nov" (day and month in the order of the language), an ISO month (`2026-11`) "Nov 26", in the page's language; any other label is shown as it is. */
export function axisLabel(label: string): string {
  const parts = /^(\d{4})-(\d{2})(?:-(\d{2}))?$/.exec(label);
  if (!parts) return label;
  const year = Number(parts[1]), month = Number(parts[2]);
  if (month < 1 || month > 12) return label;
  const lang = document.documentElement.lang || "en";
  // the locale orders day and month itself
  if (parts[3]) return new Intl.DateTimeFormat(lang, { day: "numeric", month: "short", timeZone: "UTC" }).format(new Date(Date.UTC(year, month - 1, Number(parts[3]))));
  const name = new Intl.DateTimeFormat(lang, { month: "short", timeZone: "UTC" }).format(new Date(Date.UTC(year, month - 1, 1)));
  return `${name} ${parts[1]!.slice(2)}`;
}

/** The compact chart: bars or a line over a zero baseline, height from `--dashboard-chart-height`, and the points as a list for assistive technology. */
export function renderDashboardChart(chart: DashboardChart, locale: NumericLocale): HTMLElement {
  const figure = el("figure", undefined, "dashboard-card-chart");
  const caption = el("figcaption", chart.label, "dashboard-card-chart-label");
  figure.append(caption);
  if (!chart.points.length) { figure.append(el("p", "No figures for this period.", "dashboard-card-chart-empty")); return figure; }
  const values = chart.points.map(point => number(point.value));
  const max = Math.max(0, ...values), min = Math.min(0, ...values);
  // A fixed coordinate box stretched to the figure; strokes keep their width (`vector-effect` in the stylesheet).
  const width = chart.points.length * 10, height = 100, band = width / chart.points.length;
  const y = (value: number): number => height - ((value - min) / (max - min || 1)) * height;
  const drawing = svg("svg", { viewBox: `0 0 ${width} ${height}`, preserveAspectRatio: "none", "aria-hidden": "true", focusable: "false" }, `dashboard-card-chart-${chart.type}`);
  drawing.append(svg("line", { x1: 0, x2: width, y1: y(0), y2: y(0) }, "graph-axis"));
  if (chart.type === "bar") {
    values.forEach((value, index) => {
      const top = Math.min(y(value), y(0)), bottom = Math.max(y(value), y(0));
      drawing.append(svg("rect", { x: band * index + band * 0.15, y: top, width: band * 0.7, height: value === 0 ? 0 : Math.max(bottom - top, 0.5) }, `graph-bar dashboard-card-tone-${chart.points[index]!.tone}`));
    });
  } else {
    // One line, one colour: a point's tone is read in the list below (and on bars).
    drawing.append(svg("polyline", { points: values.map((value, index) => `${band * (index + 0.5)},${y(value)}`).join(" ") }, "graph-line dashboard-card-tone-neutral"));
  }
  const axis = el("div", undefined, "dashboard-card-chart-axis");
  axis.setAttribute("aria-hidden", "true");
  axis.style.gridTemplateColumns = `repeat(${chart.points.length}, minmax(0, 1fr))`;
  // Every `step`-th point gets its label, counted from the last one (today, the current month), each under its own point.
  const count = chart.points.length, step = Math.ceil(count / AXIS_LABELS);
  chart.points.forEach((point, index) => {
    if ((count - 1 - index) % step !== 0) return;
    const tick = el("span", axisLabel(point.label));
    tick.style.gridColumn = String(index + 1);
    if (step > 1) tick.style.justifySelf = index === 0 ? "start" : index === count - 1 ? "end" : "center";
    axis.append(tick);
  });
  const points = el("ul", undefined, "visually-hidden");
  points.append(...chart.points.map((point, index) => el("li", `${point.label}: ${formatNumeric(values[index]!, "float", locale)}${point.tone === "neutral" ? "" : ` (${point.tone})`}`)));
  figure.append(drawing, axis, points);
  return figure;
}

/** Draws a parsed card; `open` runs the application's action opener. `scope` is the widget's lifetime: after it
 *  aborts, a currency answer draws and reports nothing. */
export function renderDashboardCard(card: DashboardCard, environment: WidgetEnvironment, scope?: AbortSignal): HTMLElement {
  const root = el("div", undefined, "dashboard-card");
  const { numericLocale: locale, openAction } = environment;
  if (!locale) throw new ClientError("unsupported", "The dashboard card needs the user's number format.");
  if (!openAction) throw new ClientError("unsupported", "The dashboard card opens actions only on a kanban card.");
  const open = (request: ActionRequest): void => environment.run(() => openAction(request));
  if (card.buttons.length) {
    const row = el("div", undefined, "dashboard-card-buttons");
    // Primary first, each group in the order the server sent.
    for (const button of [...card.buttons.filter(item => item.primary), ...card.buttons.filter(item => !item.primary)]) {
      row.append(createButtonControl({ label: button.label, rank: button.primary ? "primary" : "secondary", size: "small",
        action: () => open({ xmlId: button.action, viewMode: button.viewMode, domain: [], context: button.context }) }));
    }
    root.append(row);
  }
  if (card.stats.length) {
    const stats = el("ul", undefined, "dashboard-card-stats");
    for (const stat of card.stats) {
      const control = createButtonControl({ label: stat.label, rank: "unstyled", className: `dashboard-card-stat dashboard-card-tone-${stat.tone}`,
        action: () => open({ xmlId: stat.action, viewMode: "list", domain: stat.domain, context: stat.context }) });
      control.prepend(el("span", formatNumeric(stat.count, "integer", locale), "dashboard-card-stat-count"));
      if (stat.amount !== null && stat.currencyId !== null) control.append(amount(stat.amount, stat.currencyId, environment, locale, scope));
      const item = el("li");
      item.append(control);
      stats.append(item);
    }
    root.append(stats);
  }
  if (card.chart) root.append(renderDashboardChart(card.chart, locale));
  return root;
}

/** Failed currency reads already reported. Every figure of a view waits on the one cached read (`CurrencyDisplayCache`),
 *  so they all get the same error object: the user is told once, not once per figure. */
const reportedFailures = new WeakSet<object>();

/** An amount in its currency (symbol, position and decimals read through the view's shared currency cache). The read
 *  itself stays under the view's signal, since other cards wait on it; `scope` only decides whether this card still listens. */
function amount(value: string, currencyId: number, environment: WidgetEnvironment, locale: NumericLocale, scope?: AbortSignal): HTMLElement {
  const host = el("span", "…", "dashboard-card-stat-amount");
  host.setAttribute("aria-busy", "true");
  if (!environment.currencies) throw new ClientError("unsupported", "The dashboard card needs the view's currency cache.");
  const left = (): boolean => scope?.aborted === true || environment.signal?.aborted === true;
  void environment.currencies.get(currencyId, environment.rpc, environment.context, environment.signal).then(currency => {
    if (left()) return;
    const figure = number(value);
    validateMonetaryAmount(figure, currency.digits);
    const text = el("span", formatNumeric(figure, "float", locale, currency.digits)), symbol = el("span", currency.symbol, "field-currency-symbol");
    const presentation = el("span", undefined, "field-currency-value");
    presentation.append(...(currency.position === "before" ? [symbol, text] : [text, symbol]));
    host.replaceChildren(presentation);
    host.removeAttribute("aria-busy");
  }).catch(error => {
    if (left()) return;
    host.textContent = "Amount unavailable";
    host.removeAttribute("aria-busy");
    if (typeof error === "object" && error !== null) {
      if (reportedFailures.has(error)) return;
      reportedFailures.add(error);
    }
    environment.report(error);
  });
  return host;
}

const renderError = (message: string): HTMLElement => el("p", message, "dashboard-card-error");

/** The registered render: an empty body without data, the refusal by name on bad data, else the card. */
export function dashboardCardWidget(environment: WidgetEnvironment): WidgetHandle {
  let node: HTMLElement;
  // The widget's own lifetime: a card replaced by a search or a page change stops listening to its currency reads.
  const scope = new AbortController();
  try {
    const card = parseDashboardCard(environment.value);
    node = card ? renderDashboardCard(card, environment, scope.signal) : el("div", undefined, "dashboard-card dashboard-card-empty");
  } catch (error) {
    if (!(error instanceof ClientError)) throw error;
    node = renderError(error.message);
  }
  return { node, dispose: () => scope.abort() };
}

registerFieldWidget("card.dashboard_card", { kinds: ["text"], render: dashboardCardWidget });
// The plain key admits the widget name in view descriptions (metadata.ts); outside a card it says where it belongs.
registerFieldWidget("dashboard_card", { kinds: ["text"], render: () => ({ node: renderError("The dashboard card widget is drawn on kanban cards only."), dispose: () => {} }) });

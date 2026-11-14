/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Field } from "./metadata.js";
import { ClientError } from "./transport.js";
import { button, el, errorMessage } from "./dom.js";
import { decodeAst, parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
export interface DashboardMetric {
  kind: "count" | "kpi";
  name: string;
  label: string;
  color: string | null;
}
export interface DashboardConfig {
  version: 1;
  revision: string;
  items: readonly DashboardMetric[];
}
function bad(message = "Invalid dashboard declaration."): never {
  throw new ClientError("server", message);
}
function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) bad();
  return value as Record<string, unknown>;
}
function closed(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some((k) => !Object.hasOwn(value, k))) bad();
}
function text(value: unknown, max = 4096): string {
  if (typeof value !== "string" || !value.trim() || new TextEncoder().encode(value).length > max) bad();
  return value;
}
function identity(value: unknown): string {
  const name = text(value, 256);
  if (!/^[a-zA-Z0-9_]+$/.test(name)) bad();
  return name;
}
/** Independently admit the dashboard node; metadata never grants data access. */
export function readDashboard(raw: unknown, fields: ReadonlyMap<string, Field>, model: string): DashboardConfig {
  const config = object(raw);
  closed(config, [...(Object.hasOwn(config, "kind") ? ["kind"] : []), "version", "revision", "items"]);
  if (
    (Object.hasOwn(config, "kind") && config["kind"] !== "dashboard") ||
    config["version"] !== 1 ||
    typeof config["revision"] !== "string" ||
    !/^[a-f0-9]{64}$/.test(config["revision"])
  )
    bad();
  const items = config["items"];
  if (!Array.isArray(items) || items.length < 1 || items.length > 32) bad();
  const names = new Set<string>();
  return {
    version: 1,
    revision: config["revision"],
    items: items.map((raw) => {
      const item = object(raw),
        kind = item["kind"],
        name = identity(item["name"]),
        label = text(item["label"]);
      if (names.has(name)) bad();
      names.add(name);
      if (kind === "count") {
        closed(item, ["kind", "name", "label", "domain", "filter", "color"]);
        if ((item["domain"] === null) === (item["filter"] === null)) bad();
        if (item["filter"] !== null) text(item["filter"], 256);
        if (
          item["color"] !== null &&
          (typeof item["color"] !== "string" || !["info", "success", "warning", "danger"].includes(item["color"]))
        )
          bad();
      } else if (kind === "kpi") {
        closed(item, ["kind", "name", "label", "domain", "field", "date_field", "aggregate"]);
        if (typeof item["aggregate"] !== "string" || !["sum", "avg", "min", "max"].includes(item["aggregate"])) bad();
        for (const key of ["field", "date_field"]) {
          const ref = object(item[key]);
          closed(ref, ["model", "name"]);
          const field = fields.get(text(ref["name"], 256));
          if (
            ref["model"] !== model ||
            !field ||
            !(key === "field" ? ["integer", "float"] : ["date", "datetime"]).includes(field.kind)
          )
            bad();
        }
      } else return bad();
      if (item["domain"] !== null) decodeAst(parseBoundedJson(JSON.stringify(item["domain"])));
      return {
        kind,
        name,
        label,
        color: kind === "count" ? (item["color"] as string | null) : null,
      };
    }),
  };
}
function numeric(value: unknown): string | null {
  if (value === null) return null;
  if (typeof value !== "string" || value.length > 256 || !/^[-+]?\d+(?:\.\d+)?$/.test(value))
    bad("Invalid dashboard numeric value.");
  return value;
}
function domain(value: unknown): unknown[] {
  if (!Array.isArray(value) || value.length > 4096 || JSON.stringify(value).length > 65536)
    bad("Invalid dashboard count domain.");
  return structuredClone(value);
}
/** A disposable, view-neutral controller. The host owns RPC, scope and search facets. */
/** Drops from the address's `search_filters` the dashboard cards that are not registered now, so a strip that failed to read (or is not shown)
 *  never makes the list refuse to open. Other ids are left for the search model to judge. */
export function dropAbsentDashboardFilters(params: URLSearchParams, known: ReadonlySet<string>): void {
  const raw = params.get("search_filters");
  if (raw === null) return;
  let ids: unknown;
  try { ids = JSON.parse(raw); } catch { return; } // malformed: the search model refuses it by name
  if (!Array.isArray(ids)) return;
  const kept = ids.filter(id => !(typeof id === "string" && id.startsWith("dashboard_") && !known.has(id)));
  if (kept.length === ids.length) return;
  if (kept.length) params.set("search_filters", JSON.stringify(kept)); else params.delete("search_filters");
}

export function mountDashboard(options: {
  config: DashboardConfig;
  read: (domain: unknown[], period: string) => Promise<unknown>;
  apply: (name: string, domain: unknown[]) => void;
  selected: (name: string) => boolean;
  current?: () => boolean;
}) {
  const element = el("section", undefined, "dashboard-strip");
  element.setAttribute("aria-label", "Dashboard");
  const controls = el("div", undefined, "dashboard-controls"),
    select = document.createElement("select");
  select.setAttribute("aria-label", "Dashboard period");
  select.disabled = true;
  controls.append(select);
  const cells = el("div", undefined, "dashboard-items"),
    status = el("div", undefined, "dashboard-status");
  status.setAttribute("role", "status");
  element.append(controls, cells, status);
  const handles = options.config.items.map((item) => {
    const cell =
      item.kind === "count"
        ? button(
            item.label,
            () => {
              const found = domains.get(item.name);
              if (found) options.apply(item.name, found);
            },
            "dashboard-count",
          )
        : el("div", undefined, "dashboard-kpi");
    if (item.color) cell.dataset["role"] = item.color;
    cell.dataset["dashboardItem"] = item.name;
    const value = el("strong", "—", "dashboard-value"),
      label = el("span", item.label, "dashboard-label"),
      change = el("span", undefined, "dashboard-change");
    cell.replaceChildren(value, label, change);
    cells.append(cell);
    if (cell instanceof HTMLButtonElement) {
      cell.disabled = true;
      cell.setAttribute("aria-pressed", String(options.selected(item.name)));
    }
    return { item, cell, value, change };
  });
  let generation = 0,
    disposed = false,
    period = "",
    lastKey: string | undefined,
    lastDomain: unknown[] = [],
    domains = new Map<string, unknown[]>();
  const selected = () => {
    for (const h of handles)
      if (h.cell instanceof HTMLButtonElement)
        h.cell.setAttribute("aria-pressed", String(options.selected(h.item.name)));
  };
  async function refresh(scope: unknown[], force = false): Promise<void> {
    selected();
    const key = JSON.stringify([scope, period]);
    if (!force && lastKey === key) return;
    lastKey = key;
    lastDomain = structuredClone(scope);
    const focused = element.contains(document.activeElement) ? (document.activeElement as HTMLElement) : undefined;
    const mine = ++generation;
    element.setAttribute("aria-busy", "true");
    status.textContent = "Loading dashboard…";
    for (const h of handles) {
      h.value.textContent = "—";
      h.change.textContent = "";
      if (h.cell instanceof HTMLButtonElement) h.cell.disabled = true;
    }
    domains = new Map();
    try {
      const response = object(await options.read(scope, period));
      if (disposed || mine !== generation || options.current?.() === false) return;
      if (response["version"] !== 1 || typeof response["hidden"] !== "boolean") bad("Invalid dashboard response.");
      if (response["hidden"]) {
        closed(response, ["version", "hidden"]);
        element.hidden = true;
        return;
      }
      closed(response, ["version", "hidden", "revision", "period", "periods", "values"]);
      if (response["revision"] !== options.config.revision) bad("Dashboard declaration changed. Reload this view.");
      const choices = response["periods"];
      if (!Array.isArray(choices) || choices.length < 1 || choices.length > 16) bad("Invalid dashboard periods.");
      const seen = new Set<string>(),
        newOptions: HTMLOptionElement[] = [];
      for (const raw of choices) {
        const choice = object(raw);
        closed(choice, ["id", "label", "kind", "days"]);
        const id = identity(choice["id"]);
        if (seen.has(id)) bad("Invalid dashboard periods.");
        seen.add(id);
        if (
          choice["kind"] === "rolling"
            ? !Number.isInteger(choice["days"]) || Number(choice["days"]) < 1 || Number(choice["days"]) > 3660
            : typeof choice["kind"] !== "string" ||
              !["month", "year"].includes(choice["kind"]) ||
              choice["days"] !== null
        )
          bad("Invalid dashboard periods.");
        const option = document.createElement("option");
        option.value = id;
        option.textContent = text(choice["label"], 256);
        newOptions.push(option);
      }
      const chosen = text(response["period"], 64);
      if (!seen.has(chosen)) bad("Invalid dashboard periods.");
      const values = response["values"];
      if (!Array.isArray(values) || values.length !== handles.length) bad("Invalid dashboard metric response.");
      const results = handles.map((h) => {
        const matches = values.filter((v) => object(v)["name"] === h.item.name);
        if (matches.length !== 1) bad("Invalid dashboard metric response.");
        const row = object(matches[0]);
        closed(row, h.item.kind === "count" ? ["name", "value", "domain"] : ["name", "value", "previous", "change"]);
        if (h.item.kind === "kpi") numeric(row["previous"]);
        else if (typeof row["value"] !== "string" || !/^\d+$/.test(row["value"])) bad("Invalid dashboard count.");
        return {
          value: numeric(row["value"]),
          change: h.item.kind === "kpi" ? numeric(row["change"]) : null,
          domain: h.item.kind === "count" ? domain(row["domain"]) : undefined,
        };
      });
      select.replaceChildren(...newOptions);
      select.value = chosen;
      period = chosen;
      select.disabled = false;
      element.hidden = false;
      results.forEach((row, i) => {
        const h = handles[i]!;
        h.value.textContent = row.value ?? "—";
        h.change.textContent =
          h.item.kind === "kpi"
            ? row.change === null
              ? "No previous comparison"
              : `${row.change}% vs previous period`
            : "";
        if (row.domain) {
          domains.set(h.item.name, row.domain);
          if (h.cell instanceof HTMLButtonElement) h.cell.disabled = false;
        }
      });
      selected();
      lastKey = JSON.stringify([scope, period]);
      status.textContent = "";
      status.setAttribute("role", "status");
      // Retry is replaced by the answer: the focus goes to the first card instead of falling to the page.
      if (focused && document.activeElement === document.body) (focused.isConnected ? focused : element.querySelector<HTMLElement>("button:not(:disabled)"))?.focus();
    } catch (error) {
      if (disposed || mine !== generation || options.current?.() === false) return;
      lastKey = undefined;
      status.setAttribute("role", "alert");
      status.replaceChildren(
        el("span", `Dashboard unavailable: ${errorMessage(error)}`),
        button("Retry", () => {
          void refresh(lastDomain, true);
        }),
      );
      // A failed Retry keeps the focus on the (new) Retry button instead of dropping it to the page.
      if (focused && document.activeElement === document.body) status.querySelector("button")?.focus();
    } finally {
      if (!disposed && mine === generation) element.setAttribute("aria-busy", "false");
    }
  }
  select.addEventListener("change", () => {
    period = select.value;
    void refresh(lastDomain, true);
  });
  return {
    element,
    refresh,
    domains: () => new Map(domains),
    dispose: () => {
      disposed = true;
      generation++;
      domains.clear();
    },
  };
}

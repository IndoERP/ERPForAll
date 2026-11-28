/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon, type ClientActionEnvironment, type ClientActionHandle } from "@erpforall/client";
import { renderBarChart, renderLineChart, type ChartPoint } from "./canvas/canvas-chart.js";

function object(raw: unknown): Record<string, unknown> {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error("Invalid report response.");
  return raw as Record<string, unknown>;
}
function element<K extends keyof HTMLElementTagNameMap>(tag: K, text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  return node;
}
function display(value: unknown): string {
  if (Array.isArray(value) && value.length === 2 && typeof value[1] === "string") return value[1];
  if (value === null) return "—";
  if (typeof value === "number" && Number.isFinite(value)) return value.toLocaleString();
  if (typeof value === "string" || typeof value === "boolean") return String(value);
  throw new Error("Invalid report cell.");
}
function list(raw: unknown, allowEmpty = false): string[] {
  if (!Array.isArray(raw) || (!allowEmpty && !raw.length) || !raw.every(value => typeof value === "string" && value.length)) {
    throw new Error("Invalid report fields.");
  }
  return raw as string[];
}

async function panel(env: ClientActionEnvironment, raw: unknown, company: number, signal: AbortSignal) {
  const spec = object(raw);
  const model = spec["model"], title = spec["title"];
  const args = object(spec["args"]);
  const domain = args["domain"];
  const groups = list(args["groupby"], true), measures = list(args["aggregates"]).filter(field => field !== "__count");
  const context = { allowed_company_ids: [company] };
  const active = object(args["context"])["allowed_company_ids"];
  if (typeof model !== "string" || typeof title !== "string" || !Array.isArray(domain) ||
      !Array.isArray(active) || active.length !== 1 || active[0] !== company) {
    throw new Error("Reopen this report in its current company.");
  }
  const flat = groups.length !== 1 || String(args["order"] ?? "").includes(":");
  const method = flat ? "formatted_read_group" : "web_read_group";
  const limit = spec["limit"];
  if (spec["method"] !== method || typeof limit !== "number" || !Number.isSafeInteger(limit) ||
      limit < 1 || args["limit"] !== limit + Number(flat)) throw new Error("Invalid report query.");
  const answer = await env.rpc.call(model, method, { ...args, context }, { signal });
  const values = flat ? answer : object(answer)["groups"];
  if (!Array.isArray(values) || values.length > limit + Number(flat)) throw new Error("Invalid grouped report.");
  const rows = values.slice(0, limit);
  const total = flat ? null : object(answer)["length"];
  if (!flat && (typeof total !== "number" || !Number.isSafeInteger(total) || total < rows.length)) throw new Error("Invalid group count.");
  const labels = object(spec["labels"]);
  const section = element("section"); section.className = "ai-insight-panel";
  section.append(element("h2", title));
  const source = element("button", typeof spec["source"] === "string" ? spec["source"] : model);
  source.type = "button"; source.className = "ai-workspace-reference"; source.title = "Open source records";
  const drillDown = (extra: unknown[]) => {
    if (signal.aborted) return;
    env.run(() => env.open({ type: "ir.actions.act_window", name: title, res_model: model, views: [[false, "list"], [false, "form"]],
      domain: [...domain, ...extra], target: "current" }, { sourceCompanyId: company }));
  };
  source.onclick = () => drillDown([]);
  const provenance = element("div"); provenance.className = "ai-insight-source";
  provenance.append(source);
  if (domain.length) {
    const details = element("details");
    details.append(element("summary", "Filters"), element("code", JSON.stringify(domain)));
    provenance.append(details);
  }
  section.append(provenance);
  if (!rows.length) { section.append(element("p", "No matching records.")); return section; }
  const table = element("table"); table.setAttribute("aria-label", title);
  const head = table.createTHead().insertRow();
  const axis = groups[0];
  if (!axis) {
    const column = element("th", "Scope"); column.scope = "col"; head.append(column);
  }
  const series: ChartPoint[] = [];
  const visualization = spec["visualization"] ?? "table";
  if (!["table","bar","line"].includes(String(visualization))) throw new Error("Unsupported report visualization.");
  for (const key of [...groups, ...measures]) {
    const label = labels[key];
    if (typeof label !== "string") throw new Error("Report column label is missing.");
    const th = element("th", label); th.scope = "col"; head.append(th);
  }
  const body = table.createTBody();
  for (const item of rows) {
    const row = object(item);
    const tr = body.insertRow();
    const extra = row["__extra_domain"];
    if (!Array.isArray(extra)) throw new Error("This result has no source domain.");
    const groupLabel = axis ? display(row[axis]) : "All matching records";
    if (visualization !== "table") {
      const amount = row[measures[0]!];
      const metric = typeof amount === "number" || (typeof amount === "string" && amount.trim()) ? Number(amount) : NaN;
      if (Number.isFinite(metric)) {
        series.push({label: groups.length ? groups.map(key => display(row[key])).join(" · ") : groupLabel, value:metric});
      } else if (amount !== false && amount !== null) {
        throw new Error("This measure has no numeric chart value.");
      }
    }
    const group = element("button", groupLabel); group.type = "button"; group.className = "ai-workspace-reference";
    group.onclick = () => drillDown(extra);
    const heading = element("th"); heading.scope = "row"; heading.append(group); tr.append(heading);
    for (const key of [...groups.slice(1), ...measures]) {
      tr.insertCell().textContent = display(row[key]);
    }
  }
  if (series.length) {
    const chart = visualization === "line" ? renderLineChart : renderBarChart;
    section.append(chart({title: `${title} · ${String(labels[measures[0]!] ?? measures[0])}`, series}));
  }
  const region = element("div"); region.className = "ai-insight-table"; region.append(table); section.append(region);
  if (typeof total === "number" && rows.length < total) section.append(element("p", `Showing ${rows.length} of ${total} groups.`));
  if (flat && values.length > limit) section.append(element("p", `Showing the first ${limit} ${limit === 1 ? "group" : "groups"}.`));
  return section;
}

/** Open under the reader's authenticated session and revalidate every time the data is refreshed. */
export async function savedInsight(env: ClientActionEnvironment): Promise<ClientActionHandle> {
  const id = env.action.params["insight"], company = env.currentCompanyId;
  if (typeof id !== "number" || !Number.isSafeInteger(id) || id < 1 || !company) throw new Error("Select a saved insight in your current company.");
  const root = element("section"); root.className = "ai-insight";
  const head = element("header"); const title = element("h1", env.action.name);
  const refresh = element("button"); refresh.type = "button"; refresh.className = "erp-button";
  refresh.setAttribute("aria-label", "Refresh insight"); refresh.append(icon("refresh-cw"), document.createTextNode("Refresh"));
  head.append(title, refresh);
  const contents = element("div"); contents.className = "ai-insight-grid"; root.append(head, contents); env.host.append(root);
  let active: AbortController | undefined;
  async function load() {
    active?.abort();
    const controller = new AbortController(); active = controller; const { signal } = controller;
    refresh.disabled = true; contents.replaceChildren(); contents.setAttribute("aria-busy", "true");
    try {
      const def = object(await env.rpc.call("ai.insight", "erp_analysis_queries", { ids: [id], context: { allowed_company_ids: [company] } }, { signal }));
      if (signal.aborted) return;
      if (typeof def["name"] !== "string" || !Array.isArray(def["panels"]) || !def["panels"].length || def["panels"].length > 8) throw new Error("Invalid saved dashboard.");
      title.textContent = def["name"];
      const sections = await Promise.all(def["panels"].map(raw => panel(env, raw, company!, signal)));
      if (!signal.aborted) contents.replaceChildren(...sections);
    } catch (error) {
      if (!signal.aborted) {
        const failure = element("p", error instanceof Error ? error.message : "Unable to load the report.");
        failure.setAttribute("role", "alert"); contents.replaceChildren(failure);
      }
    } finally {
      if (!signal.aborted) { refresh.disabled = false; contents.removeAttribute("aria-busy"); }
    }
  }
  refresh.onclick = () => { void load(); };
  await load();
  return { dispose() { active?.abort(); root.remove(); } };
}

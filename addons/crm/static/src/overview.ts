/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage, icon, type ClientActionEnvironment, type ClientActionHandle } from "@erpforall/client";

const STAGES = 12;
const FOLLOW_UPS = 8;
type Domain = unknown[][];

function object(raw: unknown): Record<string, unknown> {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error("Invalid CRM overview response.");
  return raw as Record<string, unknown>;
}
function count(raw: unknown): number {
  if (typeof raw !== "number" || !Number.isSafeInteger(raw) || raw < 0) throw new Error("Invalid CRM count.");
  return raw;
}
function relation(raw: unknown): [number, string] {
  if (!Array.isArray(raw) || raw.length !== 2 || !count(raw[0]) || typeof raw[1] !== "string" || !raw[1]) {
    throw new Error("Invalid CRM pipeline or stage.");
  }
  return [raw[0] as number, raw[1]];
}
function kindCounts(raw: unknown) {
  const { groups, length } = object(raw);
  if (!Array.isArray(groups) || groups.length !== length || groups.length > 2) throw new Error("Invalid CRM totals.");
  const result = new Map<string, number>();
  for (const item of groups) {
    const { kind, __count } = object(item);
    if ((kind !== "lead" && kind !== "opportunity") || result.has(kind)) throw new Error("Unexpected CRM record kind.");
    result.set(kind, count(__count));
  }
  return { leads: result.get("lead") ?? 0, opportunities: result.get("opportunity") ?? 0 };
}

export async function overview(env: ClientActionEnvironment): Promise<ClientActionHandle> {
  const company = env.currentCompanyId, uid = env.userId;
  const openWizard = env.openWizard, openAction = env.openAction;
  if (!company || !uid || !openWizard || !openAction) throw new Error("CRM Overview requires an active salesperson, company and navigation services.");
  const context = { allowed_company_ids: [company], lang: env.editorLocale.lang, tz: env.editorLocale.tz };
  const companyDomain: Domain = [["company_id", "=", company]];
  const open = [...companyDomain, ["state", "=", "open"]];
  const opportunities = [...open, ["kind", "=", "opportunity"]];
  const due = [...open, ["user_id", "=", uid], ["next_activity_date", "!=", false], ["next_activity_date", "<=", "today"]];
  const locale = env.editorLocale.lang.replaceAll("_", "-");
  const numbers = new Intl.NumberFormat(locale);
  const dates = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone: "UTC" });

  let disposed = false, reading: AbortController | undefined, submitting = false;
  const root = el("section", undefined, "crm-overview");
  root.setAttribute("aria-label", "CRM Overview");
  const submit = button("Submit Lead", () => env.run(async () => {
    if (submitting || disposed) return;
    submitting = true;
    submit.setAttribute("aria-disabled", "true");
    try {
      if (await openWizard("crm.submit_lead_window") && !disposed) await refresh();
    } finally {
      submitting = false;
      submit.removeAttribute("aria-disabled");
      if (!disposed) submit.focus({ preventScroll: true });
    }
  }), "erp-button erp-button-primary");
  submit.prepend(icon("plus"));
  const reload = button("Refresh", () => env.run(refresh), "erp-button");
  reload.prepend(icon("refresh-cw"));
  env.controls.append(submit, reload);
  env.host.append(root);

  // The installed actions supply their own record views, kind defaults and base domains.
  const showLeads = () => env.run(() => openAction({
    xmlId: "crm.lead_window", viewMode: "list", domain: companyDomain, context: {},
  }));
  const showOpportunities = (stage?: number) => env.run(() => openAction({
    xmlId: "crm.pipeline_window", viewMode: "list", domain: stage === undefined ? companyDomain : [...companyDomain, ["stage_id", "=", stage]], context: {},
  }));
  const showFollowUps = () => env.run(() => env.open({
    type: "ir.actions.act_window", name: "Follow-ups due", res_model: "crm.lead", views: [[false, "list"], [false, "form"]], domain: due, target: "current",
  }));
  function metric(label: string, value: number, show: () => void) {
    const control = button("", show, "crm-metric");
    control.append(el("span", label), el("strong", numbers.format(value)));
    return control;
  }
  function section(title: string) {
    const container = el("section", undefined, "crm-overview-panel");
    container.append(el("h2", title));
    return container;
  }
  function pipeline(raw: unknown) {
    if (!Array.isArray(raw) || raw.length > STAGES + 1) throw new Error("Invalid CRM stage list.");
    const panel = section("Pipeline by stage");
    if (!raw.length) panel.append(el("p", "No open opportunities.", "crm-empty"));
    let currentPipeline: number | undefined;
    for (const item of raw.slice(0, STAGES)) {
      const row = object(item), [pipelineId, pipelineName] = relation(row["pipeline_id"]);
      const [stageId, stageName] = relation(row["stage_id"]);
      if (pipelineId !== currentPipeline) panel.append(el("h3", pipelineName));
      currentPipeline = pipelineId;
      const link = button("", () => showOpportunities(stageId), "crm-overview-row");
      link.append(el("span", stageName), el("strong", numbers.format(count(row["__count"]))));
      panel.append(link);
    }
    if (raw.length > STAGES) {
      panel.append(button("View all open opportunities", () => showOpportunities(), "crm-overview-more"));
    }
    return panel;
  }
  function queue(raw: unknown) {
    const response = object(raw), total = count(response["length"]), rows = response["records"];
    if (!Array.isArray(rows) || rows.length !== Math.min(total, FOLLOW_UPS)) throw new Error("Invalid CRM follow-up queue.");
    const panel = section("My follow-ups");
    if (!rows.length) panel.append(el("p", "No follow-ups due.", "crm-empty"));
    for (const item of rows) {
      const { id, name, next_step: step, next_activity_date: date } = object(item);
      if (!count(id) || typeof name !== "string" || !name.trim() || typeof date !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(date)) {
        throw new Error("Invalid CRM follow-up record.");
      }
      const link = button("", () => env.run(() => env.open({
        type: "ir.actions.act_window", name: "CRM record", res_model: "crm.lead", res_id: id, views: [[false, "form"]], target: "current",
      })), "crm-overview-row");
      const detail = el("span", undefined, "crm-follow-up");
      detail.append(el("strong", name));
      if (typeof step === "string" && step.trim()) detail.append(el("span", step));
      const time = el("time", dates.format(new Date(date + "T00:00:00Z")));
      time.dateTime = date;
      link.append(detail, time);
      panel.append(link);
    }
    if (total > FOLLOW_UPS) panel.append(button("View all follow-ups", showFollowUps, "crm-overview-more"));
    return { total, panel };
  }
  async function refresh() {
    reading?.abort();
    const controller = new AbortController();
    reading = controller;
    const { signal } = controller;
    root.setAttribute("aria-busy", "true");
    try {
      const [totals, stageRows, followUps] = await Promise.all([
        env.rpc.call("crm.lead", "web_read_group", { domain: open, groupby: ["kind"], aggregates: ["__count"], limit: 2, context }, { signal }),
        env.rpc.call("crm.lead", "formatted_read_group", { domain: opportunities, groupby: ["pipeline_id", "stage_id"],
          aggregates: ["__count"], limit: STAGES + 1, context }, { signal }),
        env.rpc.call("crm.lead", "web_search_read", { domain: due, specification: { name: {}, next_step: {}, next_activity_date: {} },
          order: "next_activity_date asc, id asc", offset: 0, limit: FOLLOW_UPS, context }, { signal }),
      ]);
      if (signal.aborted) return;
      const values = kindCounts(totals), follow = queue(followUps);
      const metrics = el("div", undefined, "crm-metrics");
      metrics.append(metric("Open leads", values.leads, showLeads), metric("Open opportunities", values.opportunities, () => showOpportunities()),
        metric("Follow-ups due", follow.total, showFollowUps));
      const details = el("div", undefined, "crm-overview-panels");
      details.append(pipeline(stageRows), follow.panel);
      root.replaceChildren(metrics, details);
    } catch (error) {
      if (signal.aborted) return;
      const failure = el("p", errorMessage(error), "crm-overview-error");
      failure.setAttribute("role", "alert");
      root.replaceChildren(failure);
    } finally {
      if (!signal.aborted) root.removeAttribute("aria-busy");
    }
  }
  await refresh();
  return { dispose() { disposed = true; reading?.abort(); submit.remove(); reload.remove(); root.remove(); } };
}

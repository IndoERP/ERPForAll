/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, it, vi } from "vitest";
import type { ClientActionEnvironment } from "@erpforall/client";
import { savedInsight } from "../src/saved-insight.js";

function setup() {
  const query = {
    title: "Orders by stage", model: "sale.order", source: "Sales Order", method: "web_read_group", limit: 10, labels: {state:"Status","id:count":"Orders"},
    args: { domain: [["active", "=", true]], groupby: ["state"], aggregates: ["id:count"], limit: 10, context: { allowed_company_ids: [1] } },
  };
  const call = vi.fn().mockResolvedValueOnce({ name: "Sales", panels: [query] })
    .mockResolvedValueOnce({ groups: [{ state: "sale", "id:count": 2, __count: 2, __extra_domain: [["state", "=", "sale"]] }], length: 1 });
  const host = document.createElement("main");
  const open = vi.fn();
  const env = { host, rpc: { call }, run: (job: () => Promise<void>) => void job(), open,
    currentCompanyId: 1, action: { params: { insight: 7 } } } as unknown as ClientActionEnvironment;
  return { host, open, env, call, query };
}

it("loads the saved definition, reads live groups and opens contributing records", async () => {
  const { env, open, host, call, query } = setup();
  const handle = await savedInsight(env);
  expect(call.mock.calls.map(([model, method]) => [model, method])).toEqual([
    ["ai.insight", "erp_analysis_queries"], ["sale.order", "web_read_group"],
  ]);
  expect(call.mock.calls[1]?.[2]).toEqual(query.args);
  expect(host.querySelector("tbody")?.textContent).toContain("sale");
  host.querySelector<HTMLButtonElement>("tbody button")!.click();
  expect(open).toHaveBeenCalledWith(expect.objectContaining({ res_model: "sale.order",
    domain: [["active", "=", true], ["state", "=", "sale"]], context: { allowed_company_ids: [1] } }));
  handle.dispose();
  expect(host.childElementCount).toBe(0);
});

it("shows a source access error and clears stale numbers when refreshing", async () => {
  const { env, host, call, query } = setup();
  await savedInsight(env);
  expect(host.querySelector("tbody")).not.toBeNull();
  call.mockResolvedValueOnce({ name: "Sales", panels: [query] }).mockRejectedValueOnce(new Error("Source access denied"));
  host.querySelector<HTMLButtonElement>("button[aria-label='Refresh report']")!.click();
  await vi.waitFor(() => expect(host.querySelector("[role=alert]")?.textContent).toContain("Source access denied"));
  expect(host.querySelector("tbody")).toBeNull();
});

it("preserves both axes of flat groups and omits the lookahead row", async () => {
  const { env, host, call, query, open } = setup();
  call.mockReset();
  const axis = ["state","date_order:month"];
  call.mockResolvedValueOnce({name:"Sales", panels:[{ ...query, method:"formatted_read_group",limit:1,
    labels: { ...query.labels,"date_order:month":"Order month" },
    args:{...query.args,groupby:axis,limit:2,aggregates:["id:count","__count"]} }]});
  const drill = [["state","=","sale"],["date_order",">=","2026-01-01"]];
  call.mockResolvedValueOnce([{ state:"sale","date_order:month":["2026-01-01","January"], "id:count":3,__extra_domain:drill },
    {state:"draft","date_order:month":["2026-02-01","February"], "id:count":100,__extra_domain:[]}]);
  await savedInsight(env);
  expect(call.mock.calls[1]?.[1]).toBe("formatted_read_group");
  expect(host.querySelector("tbody")?.textContent).toBe("saleJanuary3");
  expect(host.textContent).toContain("Showing the first 1 group.");
  host.querySelector<HTMLButtonElement>("tbody button")!.click();
  expect(open).toHaveBeenCalledWith(expect.objectContaining({domain:[...query.args.domain,...drill]}));
});

it("renders a saved chart from the current viewer's results", async () => {
  const {env, host, call, query} = setup();
  call.mockReset();
  call.mockResolvedValueOnce({name:"Sales",panels:[{...query,visualization:"bar"}]})
    .mockResolvedValueOnce({groups:[{state:"Draft","id:count":6,__extra_domain:[]}],length:1});
  await savedInsight(env);
  expect(host.querySelector("svg[role=img]")?.getAttribute("aria-label")).toContain("Orders");
  expect(host.querySelector("rect title")?.textContent).toContain("6");
  expect(host.querySelector("tbody")?.textContent).toContain("Draft");
});

it("displays a whole-query KPI with its matching source", async () => {
  const {env, host, call, query} = setup();
  call.mockReset();
  call.mockResolvedValueOnce({name:"Sales",panels:[{...query,visualization:"table",method:"formatted_read_group",
    args:{...query.args, groupby:[], aggregates:["id:count","__count"],limit:11}}]})
    .mockResolvedValueOnce([{"id:count":42,"__count":42,__extra_domain:[]}]);
  await savedInsight(env);
  expect(host.querySelector("tbody")?.textContent).toBe("All matching records42");
});

it("refuses an analysis definition targeting another company before querying rows", async () => {
  const {env, host, call, query} = setup();
  call.mockReset().mockResolvedValueOnce({name:"Sales",panels:[{...query, args:{
    ...query.args, context:{allowed_company_ids:[2]} }}]});
  await savedInsight(env);
  expect(call).toHaveBeenCalledTimes(1);
  expect(host.querySelector("[role=alert]")?.textContent).toContain("current company");
});

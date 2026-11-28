/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import type { ClientActionEnvironment } from "@erpforall/client";
import { overview } from "../src/overview.js";

function setup(empty = false) {
  const host = document.createElement("main"), controls = document.createElement("div");
  document.body.append(controls, host);
  const open = vi.fn(async (_action: unknown) => {});
  const openAction = vi.fn(async (_action: unknown) => {});
  const openWizard = vi.fn(async (_id: string) => false);
  const call = vi.fn(async (model: string, method: string, params: Record<string, unknown>) => {
    expect(model).toBe("crm.lead");
    expect(params["context"]).toMatchObject({ allowed_company_ids: [5], lang: "en_US", tz: "Asia/Jakarta" });
    expect(params["domain"]).toContainEqual(["company_id", "=", 5]);
    if (method === "web_read_group") return empty ? { groups: [], length: 0 } : {
      groups: [{ kind: "lead", __count: 4 }, { kind: "opportunity", __count: 3 }], length: 2,
    };
    if (method === "formatted_read_group") return empty ? [] : [
      { pipeline_id: [7, "Direct"], stage_id: [11, "Negotiation"], __count: 3 },
    ];
    if (method === "web_search_read") return empty ? { records: [], length: 0 } : {
      records: [{ id: 91, name: "Service renewal", next_activity_date: "2026-11-01", next_step: "Call client" }], length: 1,
    };
    throw new Error("Unexpected RPC " + method);
  });
  const tasks: Promise<unknown>[] = [];
  const env = { host, controls, rpc: { call }, open, openAction, openWizard,
    userId: 19, currentCompanyId: 5, editorLocale: { lang: "en_US", tz: "Asia/Jakarta" },
    run: (task: () => Promise<unknown>) => { tasks.push(task()); },
  } as unknown as ClientActionEnvironment;
  const click = async (label: string) => {
    const node = [...document.querySelectorAll("button")].find(item => item.textContent?.includes(label));
    expect(node, label).toBeDefined(); node!.click(); await Promise.all(tasks);
    return node!;
  };
  return { env, host, open, openAction, openWizard, call, click };
}
afterEach(() => document.body.replaceChildren());

test("CRM counts, pipeline stages, and follow-ups open their exact scoped records", async () => {
  const s = setup();
  await overview(s.env);
  expect(s.call).toHaveBeenCalledTimes(3);
  expect(s.host.textContent).toContain("Direct");
  await s.click("Open leads");
  expect(s.openAction).toHaveBeenLastCalledWith({ xmlId: "crm.lead_window", viewMode: "list", context: {},
    domain: [["company_id", "=", 5]] });
  await s.click("Negotiation");
  expect(s.openAction).toHaveBeenLastCalledWith({ xmlId: "crm.pipeline_window", viewMode: "list", context: {},
    domain: [["company_id", "=", 5], ["stage_id", "=", 11]] });
  const queue = s.call.mock.calls.find(([, method]) => method === "web_search_read")![2];
  expect(queue).toMatchObject({ order: "next_activity_date asc, id asc", limit: 8,
    domain: expect.arrayContaining([["user_id", "=", 19], ["next_activity_date", "<=", "today"]]) });
  await s.click("Follow-ups due");
  expect(s.open).toHaveBeenLastCalledWith(expect.objectContaining({ domain: queue["domain"] }));
  await s.click("Service renewal");
  expect(s.open).toHaveBeenLastCalledWith(expect.objectContaining({ res_model: "crm.lead", res_id: 91, target: "current" }));
});

test("submitting refreshes the dashboard, cancelling restores focus and keeps the data", async () => {
  const s = setup(true);
  const handle = await overview(s.env);
  expect(s.host.textContent).toContain("No open opportunities");
  expect(s.host.textContent).toContain("No follow-ups due");
  const launch = await s.click("Submit Lead");
  expect(s.openWizard).toHaveBeenCalledWith("crm.submit_lead_window");
  expect(document.activeElement).toBe(launch);
  expect(s.call).toHaveBeenCalledTimes(3);
  s.openWizard.mockResolvedValueOnce(true);
  await s.click("Submit Lead");
  expect(s.call).toHaveBeenCalledTimes(6);
  expect(s.open).not.toHaveBeenCalled();
  handle.dispose();
  expect(document.body.querySelector("button")).toBeNull();
});

test("an invalid response shows an error and can be retried without displaying misleading zeroes", async () => {
  const s = setup(true);
  s.call.mockResolvedValueOnce({ groups: [{ kind: "lead", __count: -4 }], length: 1 });
  await overview(s.env);
  expect(s.host.querySelector('[role=alert]')).not.toBeNull();
  expect(s.host.textContent).not.toContain("No open opportunities");
  await s.click("Refresh");
  expect(s.host.querySelector('[role=alert]')).toBeNull();
  expect(s.host.textContent).toContain("No follow-ups due");
});

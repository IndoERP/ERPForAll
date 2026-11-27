/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError, registerClientActionRedirect, rpcId, t, type ClientActionRedirectEnvironment, type Route } from "@erpforall/client";

async function legacyDestination(
  { route, rpc, context, signal }: ClientActionRedirectEnvironment,
  xmlId: "approvals.requests_window" | "approvals.workflows_window",
): Promise<Route> {
  const value = await rpc.call("ir.actions.act_window", "erp_load_web_action_ref", { xml_id: xmlId, context }, { signal });
  const action = value as { wire_major?: unknown; target?: unknown; id?: unknown } | null;
  if (action?.wire_major !== 1 || action.target !== "current" || typeof action.id !== "number"
      || !Number.isSafeInteger(action.id) || action.id < 1) {
    throw new ClientError("server", t("Invalid approval action."));
  }
  return { action: String(action.id), menu: route.menu, cids: route.cids, debug: route.debug };
}

registerClientActionRedirect("approvals.requests", async environment => {
  const { route } = environment;
  const tab = /^(review|mine|reviewed)(?::[1-9]\d{0,8})?$/.exec(route.list_tab ?? "review")?.[1];
  const filter = tab === "review" ? "to_review" : tab === "mine" ? "is_mine" : tab === "reviewed" ? "is_reviewed" : undefined;
  if (!filter) throw new ClientError("validation", t("Unknown approval queue."));
  const destination = await legacyDestination(environment, "approvals.requests_window");
  return { ...destination, search_filters: JSON.stringify([filter]), ...(route.id ? { id: route.id, view_type: "form" } : {}) };
});

registerClientActionRedirect("approvals.configuration", async environment => {
  const { route, rpc, context, signal } = environment;
  const destination = await legacyDestination(environment, "approvals.workflows_window");
  if (!route.id) return destination;
  const value = await rpc.call("approval.workflow", "web_search_read", {
    domain: [["policy_id", "=", rpcId(route.id)]], specification: { id: {} }, limit: 2, context: { ...context, active_test: false },
  }, { signal });
  const rows = (value as { records?: unknown } | null)?.records;
  if (!Array.isArray(rows) || rows.length > 1) throw new ClientError("server", t("Invalid approval workflow lookup."));
  if (!rows.length) throw new ClientError("permission", t("This approval workflow is unavailable."));
  const id = (rows[0] as { id?: unknown } | null)?.id;
  if (typeof id !== "number" || !Number.isSafeInteger(id) || id < 1) {
    throw new ClientError("server", t("Invalid approval workflow identity."));
  }
  return { ...destination, id: String(id), view_type: "form" };
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeAll, expect, test, vi } from "vitest";
import { DialogService } from "../../addons/web/static/src/client/dialog-service.js";
import { waitForApproval } from "../../addons/web/static/src/client/approval.js";
import { clientActionRedirects } from "../../addons/web/static/src/client/registry.js";
import { RpcClient } from "../../addons/web/static/src/client/transport.js";
import "../../addons/approvals/static/src/requests.js";

beforeAll(() => {
  if (typeof HTMLDialogElement.prototype.showModal !== "function") {
    HTMLDialogElement.prototype.showModal = function () { this.open = true; };
    HTMLDialogElement.prototype.close = function () { this.open = false; this.dispatchEvent(new Event("close")); };
  }
});
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });
const receipt = (state: string) => ({ id: 23, state, current_step: 1, step_count: 1, current_step_name: "Preferences review" });
const answer = (status: number, value: unknown) => new Response(JSON.stringify(value), {
  status, headers: { "content-type": "application/json" },
});
function control(root: HTMLElement, name: string): HTMLButtonElement {
  const found = [...root.querySelectorAll("button")].find(b => (b.getAttribute("aria-label") ?? b.textContent) === name);
  if (!found) throw Error(`Missing button ${name}`);
  return found;
}

test("the requester repeats identical bytes with the same key after approval", async () => {
  const calls: { key: string | null; body: string }[] = [];
  let reads = 0;
  const values = { ids: [4], vals: { tz: "UTC" } };
  const signal = new AbortController().signal;
  const service = new DialogService(error => { throw error; });
  const fetch = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    if (String(url).endsWith("/erp_status")) return answer(200, ++reads === 1
      ? { ...receipt("pending"), current_step: 2, step_count: 3 }
      : receipt("approved"));
    calls.push({ key: new Headers(init?.headers).get("Idempotency-Key"), body: String(init?.body) });
    if (calls.length === 1) return answer(422, { context: { approval_key_required: true } });
    if (calls.length === 2) return answer(409, { context: { approval_request_id: 23 } });
    return answer(200, true);
  });
  const rpc: RpcClient = new RpcClient(fetch, () => {}, undefined, undefined, undefined, (id): Promise<boolean> => waitForApproval(service, rpc, id, signal));
  const done = rpc.call("res.users", "write", values, { write: true });
  await vi.waitFor(() => expect(service.active()).toBeDefined());
  expect(service.active()!.textContent).toContain("A reviewer can find this request in Approvals.");
  values.vals.tz = "Asia/Jakarta";
  control(service.active()!, "Check approval").click();
  await vi.waitFor(() => expect(service.active()!.textContent).toContain("Waiting for a reviewer. Stage 2 of 3"));
  expect(calls).toHaveLength(2);
  control(service.active()!, "Check approval").click();
  expect(await done).toBe(true);
  expect(calls[0]!.key).toBeNull();
  expect(calls[1]!.key).toMatch(/^[a-f0-9-]{36}$/);
  expect(calls[2]).toEqual(calls[1]);
  expect(calls[2]!.body).toBe(calls[0]!.body);
});

test("a refused request can be closed without replaying the write", async () => {
  const service = new DialogService(error => { throw error; });
  const signal = new AbortController().signal;
  const states = ["pending", "denied"];
  let writes = 0;
  const fetch = vi.fn(async (url: RequestInfo | URL) => {
    if (String(url).endsWith("/erp_status")) return answer(200, receipt(states.shift()!));
    writes++;
    return writes === 1
      ? answer(422, { context: { approval_key_required: true } })
      : answer(409, { context: { approval_request_id: 23 } });
  });
  const rpc: RpcClient = new RpcClient(fetch, () => {}, undefined, undefined, undefined,
    id => waitForApproval(service, rpc, id, signal));
  const done = expect(rpc.call("res.users", "write", { ids: [4], vals: { tz: "UTC" } }, { write: true }))
    .rejects.toThrow("Approval request 23 was not executed.");
  await vi.waitFor(() => expect(service.active()).toBeDefined());
  const dialog = service.active()!;
  const check = control(dialog, "Check approval");
  check.click();
  await vi.waitFor(() => expect(dialog.textContent).toContain("Waiting for a reviewer."));
  check.click();
  await vi.waitFor(() => expect(check.hidden).toBe(true));
  expect(dialog.textContent).toContain("This request is denied.");
  control(dialog, "Close").click();
  await done;
  expect(writes).toBe(2);
});

test("a requester can wait for a decision when the optional Approvals app is absent", async () => {
  const controller = new AbortController();
  const dialogs = new DialogService(error => { throw error; });
  const rpc = new RpcClient(async () => { throw Error("unexpected lookup"); }, () => {});
  const result = waitForApproval(dialogs, rpc, 23, controller.signal, false);
  expect(dialogs.active()?.textContent).toContain("After a reviewer approves the request");
  expect(dialogs.active()?.textContent).not.toContain("find this request in Approvals");
  controller.abort();
  expect(await result).toBe(false);
});

test("a legacy workflow detail resolves its revision through the authorized native workflow model", async () => {
  const fetch = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    const body = JSON.parse(String(init?.body));
    if (String(url).endsWith("erp_load_web_action_ref")) {
      expect(body.xml_id).toBe("approvals.workflows_window");
      return answer(200, { wire_major: 1, id: 36, target: "current" });
    }
    expect(String(url)).toMatch(/\/approval\.workflow\/web_search_read$/);
    expect(body.domain).toEqual([["policy_id", "=", 108]]);
    expect(body.context).toEqual({ allowed_company_ids: [3], active_test: false });
    return answer(200, { records: [{ id: 8 }] });
  });
  const result = await clientActionRedirects.get("approvals.configuration")({
    route: { action: "35", id: "108", view_type: "form", menu: "16", cids: ["3"] },
    rpc: new RpcClient(fetch, () => {}), context: { allowed_company_ids: [3] }, signal: new AbortController().signal,
  });
  expect(result).toMatchObject({ action: "36", id: "8", view_type: "form", menu: "16", cids: ["3"] });
});

test.each([
  ["mine", "is_mine"], ["reviewed:3", "is_reviewed"], ["review", "to_review"],
])("the legacy %s queue opens its native filter", async (tab, filter) => {
  const fetch = vi.fn(async () => answer(200, { wire_major: 1, id: 36, target: "current" }));
  const route = await clientActionRedirects.get("approvals.requests")({
    route: { action: "35", list_tab: tab }, rpc: new RpcClient(fetch, () => {}),
    context: {}, signal: new AbortController().signal,
  });
  expect(route.search_filters).toBe(JSON.stringify([filter]));
  expect(route.id).toBeUndefined();
  expect(fetch).toHaveBeenCalledOnce();
});

test("an unknown legacy queue fails before loading any action", async () => {
  const fetch = vi.fn(async () => answer(200, {}));
  await expect(clientActionRedirects.get("approvals.requests")({
    route: { action: "35", list_tab: "typo" }, rpc: new RpcClient(fetch, () => {}),
    context: {}, signal: new AbortController().signal,
  })).rejects.toMatchObject({ kind: "validation", message: "Unknown approval queue." });
  expect(fetch).not.toHaveBeenCalled();
});

test.each([
  { records: [], kind: "permission", message: "This approval workflow is unavailable." },
  { records: [{ id: 2 }, { id: 3 }], kind: "server", message: "Invalid approval workflow lookup." },
  { records: null, kind: "server", message: "Invalid approval workflow lookup." },
  { records: [{ id: 0 }], kind: "server", message: "Invalid approval workflow identity." },
])("a legacy detail refuses an unavailable or ambiguous workflow ($records)", async ({ records, kind, message }) => {
  const fetch = vi.fn(async (url: RequestInfo | URL) => String(url).endsWith("erp_load_web_action_ref")
    ? answer(200, { wire_major: 1, id: 36, target: "current" }) : answer(200, { records }));
  await expect(clientActionRedirects.get("approvals.configuration")({
    route: { action: "35", id: "108" }, rpc: new RpcClient(fetch, () => {}),
    context: {}, signal: new AbortController().signal,
  })).rejects.toMatchObject({ kind, message });
});

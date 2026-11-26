/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeAll, expect, test, vi } from "vitest";
import { DialogService } from "../../addons/web/static/src/client/dialog-service.js";
import { openApproval, waitForApproval } from "../../addons/web/static/src/client/approval.js";
import { RpcClient } from "../../addons/web/static/src/client/transport.js";

beforeAll(() => {
  if (typeof HTMLDialogElement.prototype.showModal !== "function") {
    HTMLDialogElement.prototype.showModal = function () { this.open = true; };
    HTMLDialogElement.prototype.close = function () { this.open = false; this.dispatchEvent(new Event("close")); };
  }
});
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });
const digest = "1".repeat(64);
const receipt = (state: string, canDecide: boolean) => ({
  id: 23, requester_id: 4, companies: [2], model: "res.users", method: "write", payload_sha256: digest, state, can_decide: canDecide,
});
const answer = (status: number, value: unknown) => new Response(JSON.stringify(value), {
  status, headers: { "content-type": "application/json" },
});
function control(dialog: HTMLDialogElement, name: string): HTMLButtonElement {
  const found = [...dialog.querySelectorAll("button")].find(b => b.textContent === name);
  if (!found) throw Error(`Missing button ${name}`);
  return found;
}

test("reviewer sees the bound request and confirms its displayed digest", async () => {
  const fetch = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    const body = JSON.parse(String(init?.body));
    if (String(url).endsWith("/erp_status")) {
      expect(body).toEqual({ request_id: 23 });
      return answer(200, receipt("pending", true));
    }
    expect(String(url)).toMatch(/\/erp_decide$/);
    expect(body).toEqual({ request_id: 23, approve: true, payload_sha256: digest });
    return answer(200, receipt("approved", false));
  });
  const service = new DialogService(error => { throw error; });
  openApproval(service, new RpcClient(fetch, () => {}), new AbortController().signal);
  const dialog = service.active()!;
  const input = dialog.querySelector("input")!;
  input.value = "23";
  expect(control(dialog, "Approve").hidden).toBe(true);
  control(dialog, "Open request").click();
  await vi.waitFor(() => expect(control(dialog, "Approve").hidden).toBe(false));
  expect(dialog.textContent).toContain("res.users.write");
  expect(dialog.textContent).toContain(digest);
  control(dialog, "Approve").click();
  await vi.waitFor(() => expect(dialog.textContent).toContain("Status: approved"));
  expect(control(dialog, "Deny").hidden).toBe(true);
  control(dialog, "Close").click();
});

test("the requester repeats identical bytes with the same key after approval", async () => {
  const calls: { key: string | null; body: string }[] = [];
  const values = { ids: [4], vals: { tz: "UTC" } };
  const signal = new AbortController().signal;
  const service = new DialogService(error => { throw error; });
  const fetch = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    if (String(url).endsWith("/erp_status")) return answer(200, receipt("approved", false));
    calls.push({ key: new Headers(init?.headers).get("Idempotency-Key"), body: String(init?.body) });
    if (calls.length === 1) return answer(422, { context: { approval_key_required: true } });
    if (calls.length === 2) return answer(409, { context: { approval_request_id: 23 } });
    return answer(200, true);
  });
  const rpc: RpcClient = new RpcClient(fetch, () => {}, undefined, undefined, undefined, (id): Promise<boolean> => waitForApproval(service, rpc, id, signal));
  const done = rpc.call("res.users", "write", values, { write: true });
  await vi.waitFor(() => expect(service.active()).toBeDefined());
  values.vals.tz = "Asia/Jakarta";
  control(service.active()!, "Check approval").click();
  expect(await done).toBe(true);
  expect(calls[0]!.key).toBeNull();
  expect(calls[1]!.key).toMatch(/^[a-f0-9-]{36}$/);
  expect(calls[2]).toEqual(calls[1]);
  expect(calls[2]!.body).toBe(calls[0]!.body);
});

test("a reference without current review rights shows no decision buttons", async () => {
  const rpc = new RpcClient(async () => answer(200, receipt("pending", false)), () => {});
  const service = new DialogService(error => { throw error; });
  openApproval(service, rpc, new AbortController().signal);
  const dialog = service.active()!;
  dialog.querySelector("input")!.value = "23";
  control(dialog, "Open request").click();
  await vi.waitFor(() => expect(dialog.textContent).toContain("Status: pending"));
  expect(control(dialog, "Approve").hidden).toBe(true);
  expect(control(dialog, "Deny").hidden).toBe(true);
  control(dialog, "Close").click();
});

test("a refused request can be closed without replaying the write", async () => {
  const service = new DialogService(error => { throw error; });
  const signal = new AbortController().signal;
  const states = ["pending", "denied"];
  let writes = 0;
  const fetch = vi.fn(async (url: RequestInfo | URL) => {
    if (String(url).endsWith("/erp_status")) return answer(200, receipt(states.shift()!, false));
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

test("changing a reviewed number clears the decision and rejects a mismatched receipt", async () => {
  const service = new DialogService(error => { throw error; });
  const fetch = vi.fn(async () => answer(200, receipt("pending", true)));
  openApproval(service, new RpcClient(fetch, () => {}), new AbortController().signal);
  const dialog = service.active()!;
  const input = dialog.querySelector("input")!;
  input.value = "0";
  control(dialog, "Open request").click();
  expect(dialog.textContent).toContain("Enter a positive request number.");
  expect(fetch).not.toHaveBeenCalled();
  input.value = "23";
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
  await vi.waitFor(() => expect(control(dialog, "Approve").hidden).toBe(false));
  input.value = "24";
  input.dispatchEvent(new Event("input"));
  expect(control(dialog, "Approve").hidden).toBe(true);
  expect(dialog.textContent).not.toContain(digest);
  control(dialog, "Open request").click();
  await vi.waitFor(() => expect(dialog.textContent).toContain("Invalid approval receipt."));
  expect(control(dialog, "Deny").hidden).toBe(true);
  control(dialog, "Close").click();
});

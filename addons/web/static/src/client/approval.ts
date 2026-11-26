/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { DialogService } from "./dialog-service.js";
import { button, el, errorMessage } from "./dom.js";
import { ClientError, type RpcClient } from "./transport.js";

interface Receipt {
  id: number;
  state: string;
  model: string;
  method: string;
  payload_sha256: string;
  operation?: string;
  can_decide: boolean;
  requester_id: number;
  companies: number[];
}

function receipt(value: unknown, id: number): Receipt {
  const item = value as Partial<Receipt> | null;
  if (item?.id !== id || typeof item.model !== "string" || typeof item.method !== "string"
      || typeof item.payload_sha256 !== "string" || !/^[a-f0-9]{64}$/.test(item.payload_sha256)
      || typeof item.state !== "string" || !["pending", "approved", "denied", "executed", "expired"].includes(item.state)
      || !Number.isSafeInteger(item.requester_id) || !Array.isArray(item.companies) || !item.companies.every(Number.isSafeInteger)
      || typeof item.can_decide !== "boolean") {
    throw new ClientError("server", "Invalid approval receipt.");
  }
  return item as Receipt;
}

const MODEL = "ir.approval.request";

export function waitForApproval(dialogs: DialogService, rpc: RpcClient, id: number, signal: AbortSignal): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false);
  const body = el("div");
  const message = el("p", `Give your reviewer request number ${id}. Keep this window open to complete the change after approval.`);
  const status = el("p");
  status.setAttribute("role", "status");
  body.append(message, status);
  return new Promise(resolve => {
    let proceed = false;
    const footer = el("div", undefined, "actions");
    const check = button("Check approval", () => { void lookup(); }, "primary");
    footer.append(button("Close", () => handle.close()), check);
    const handle = dialogs.open({ title: `Approval request ${id}`, body, footer, signal, onClose: () => resolve(proceed) });
    async function lookup(): Promise<void> {
      check.disabled = true;
      try {
        const r = receipt(await rpc.call(MODEL, "erp_status", { request_id: id }, { signal, background: true }), id);
        if (!handle.dialog.isConnected || signal.aborted) return;
        if (r.state === "approved" || r.state === "executed") {
          proceed = true;
          handle.close();
          return;
        }
        status.textContent = r.state === "pending" ? "Waiting for a reviewer." : `This request is ${r.state}.`;
        if (r.state !== "pending") check.hidden = true;
      } catch (error) { status.textContent = errorMessage(error); }
      finally { check.disabled = false; }
    }
  });
}

export function openApproval(dialogs: DialogService, rpc: RpcClient, signal: AbortSignal): void {
  if (signal.aborted) return;
  const body = el("div");
  const input = el("input");
  input.type = "text";
  input.inputMode = "numeric";
  input.maxLength = 16;
  input.autofocus = true;
  const label = el("label", "Request number");
  label.append(input);
  const summary = el("div");
  const status = el("p");
  status.setAttribute("role", "status");
  const footer = el("div", undefined, "actions");
  const read = button("Open request", () => { void act(); });
  const approve = button("Approve", () => { void act(true); }, "primary");
  const deny = button("Deny", () => { void act(false); });
  let reviewed: Receipt | undefined;
  function reset(): void {
    reviewed = undefined;
    approve.hidden = deny.hidden = true;
    summary.replaceChildren();
    status.textContent = "";
  }
  reset();
  input.addEventListener("input", reset);
  input.addEventListener("keydown", event => {
    if (event.key === "Enter" && !read.disabled) { event.preventDefault(); read.click(); }
  });
  footer.append(button("Close", () => handle.close()), read, deny, approve);
  body.append(label, summary, status);
  const handle = dialogs.open({ title: "Review approval", body, footer, signal });
  async function act(decision?: boolean): Promise<void> {
    const id = Number(input.value);
    if (!/^[1-9][0-9]*$/.test(input.value) || !Number.isSafeInteger(id)) {
      status.textContent = "Enter a positive request number.";
      input.focus();
      return;
    }
    if (decision !== undefined && (reviewed?.id !== id || !reviewed.can_decide)) return;
    input.disabled = read.disabled = approve.disabled = deny.disabled = true;
    try {
      const method = decision === undefined ? "erp_status" : "erp_decide";
      const args = decision === undefined ? { request_id: id } : { request_id: id, approve: decision, payload_sha256: reviewed!.payload_sha256 };
      const next = receipt(await rpc.call(MODEL, method, args, { signal, write: decision !== undefined }), id);
      if (signal.aborted || !handle.dialog.isConnected) return;
      reviewed = next;
      summary.replaceChildren(el("p", `Requested by user ${next.requester_id} for companies ${next.companies.join(", ")}.`), el("p", `Operation: ${next.model}.${next.method}`), el("p", `Request digest: ${next.payload_sha256}`));
      if (next.operation) summary.append(el("p", `Execution receipt: ${next.operation}`));
      status.textContent = `Status: ${next.state}.`;
      approve.hidden = deny.hidden = next.state !== "pending" || !next.can_decide;
    } catch (error) {
      reset();
      status.textContent = errorMessage(error);
    } finally { input.disabled = read.disabled = approve.disabled = deny.disabled = false; }
  }
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { DialogService } from "./dialog-service.js";
import { button, el, errorMessage } from "./dom.js";
import { t } from "./translate.js";
import { ClientError, type RpcClient } from "./transport.js";

const STATES = ["pending", "approved", "denied", "executed", "expired"] as const;
type ApprovalState = typeof STATES[number];

interface StatusReceipt {
  id?: unknown;
  state: ApprovalState;
  current_step?: number;
  step_count?: number;
  current_step_name?: string;
}

function approvalStatus(value: unknown, id: number): StatusReceipt {
  const receipt = value as StatusReceipt | null;
  if (receipt?.id !== id || receipt.state === undefined || !STATES.includes(receipt.state)) {
    throw new ClientError("server", t("Invalid approval receipt."));
  }
  return receipt;
}

function pendingMessage(receipt: StatusReceipt): string {
  const { current_step: stage, step_count: total, current_step_name: name } = receipt;
  if (typeof stage === "number" && Number.isSafeInteger(stage) && stage > 0
      && typeof total === "number" && Number.isSafeInteger(total) && total > 1 && stage <= total
      && typeof name === "string") {
    return t("Waiting for a reviewer. Stage {stage} of {total} · {name}", { stage, total, name });
  }
  return t("Waiting for a reviewer.");
}

export function waitForApproval(dialogs: DialogService, rpc: RpcClient, id: number, signal: AbortSignal, workspaceAvailable = true): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false);
  const body = el("div");
  const message = el("p", workspaceAvailable
    ? t("A reviewer can find this request in Approvals. Keep this window open and check for approval to complete your change.")
    : t("Keep this window open. After a reviewer approves the request, select Check approval to complete your change."));
  const status = el("p");
  status.setAttribute("role", "status");
  body.append(message, status);
  return new Promise(resolve => {
    let proceed = false;
    const footer = el("div", undefined, "actions");
    const check = button(t("Check approval"), () => { void lookup(); }, "primary");
    footer.append(button(t("Close"), () => handle.close()), check);
    const handle = dialogs.open({ title: t("Approval request {id}", { id }), body, footer, signal, onClose: () => resolve(proceed && !signal.aborted) });
    async function lookup(): Promise<void> {
      if (check.disabled || check.hidden || signal.aborted) return;
      check.disabled = true;
      try {
        const value = await rpc.call("ir.approval.request", "erp_status", { request_id: id }, { signal, background: true });
        const receipt = approvalStatus(value, id);
        if (!handle.dialog.isConnected || signal.aborted) return;
        if (receipt.state === "approved" || receipt.state === "executed") {
          proceed = true;
          handle.close();
          return;
        }
        status.textContent = receipt.state === "pending" ? pendingMessage(receipt)
          : receipt.state === "denied" ? t("This request is denied.") : t("This request has expired.");
        if (receipt.state !== "pending") check.hidden = true;
      } catch (error) {
        if (!signal.aborted && handle.dialog.isConnected) status.textContent = errorMessage(error);
      } finally { check.disabled = false; }
    }
  });
}

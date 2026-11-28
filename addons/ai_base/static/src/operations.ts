/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { AssistantSession } from "./assistant-session.js";

interface RecordTarget {
  res_model: string;
  res_id: number;
}
interface WindowAction extends RecordTarget {
  type: "ir.actions.act_window";
  name: string;
  views: [false, "form"][];
  target: "current";
}
export interface Operation {
  name: string;
  model: string;
  ids: number[];
  status: string;
  approval_request_id?: number;
  result?: RecordTarget;
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function positiveId(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}
export function operations(value: unknown): Operation[] {
  if (!Array.isArray(value) || value.length > 128) throw new Error("Invalid operation receipts.");
  return value.map(row => {
    if (!object(row) || typeof row["name"] !== "string" || typeof row["model"] !== "string" ||
        typeof row["status"] !== "string" || !Array.isArray(row["ids"]) || !row["ids"].every(positiveId)) {
      throw new Error("Invalid operation receipt.");
    }
    const result: Operation = {name:row["name"], model:row["model"], ids:row["ids"], status:row["status"]};
    if (row["approval_request_id"] != null) {
      if (!positiveId(row["approval_request_id"])) throw new Error("Invalid approval reference.");
      result.approval_request_id = row["approval_request_id"];
    }
    const target = row["result"];
    if (target != null) {
      if (!object(target) || typeof target["res_model"] !== "string" || !positiveId(target["res_id"])) {
        throw new Error("Invalid operation record reference.");
      }
      result.result = {res_model:target["res_model"], res_id:target["res_id"]};
    }
    return result;
  });
}

export function renderOperations(host: HTMLElement, items: readonly Operation[] | undefined,
  chat: AssistantSession, open?: (action: WindowAction) => void) {
  for (const item of items ?? []) {
    const row = document.createElement("div");
    row.className = "ai-operation";
    const label = document.createElement("span");
    label.setAttribute("role", "status");
    label.textContent = item.name + " · " + item.status.replaceAll("_", " ");
    row.append(label);
    const addButton = (name: string, fn: (button: HTMLButtonElement) => void) => {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = name;
      button.className = "erp-button erp-button-secondary";
      button.onclick = () => fn(button);
      row.append(button);
    };
    function link(name: string, title: string, record: RecordTarget) {
      if (open) addButton(name, () => open({
        type:"ir.actions.act_window", name:title, ...record, views:[[false,"form"]], target:"current",
      }));
    }
    const target = item.result ?? (item.ids.length === 1 ? {res_model:item.model, res_id:item.ids[0]!} : undefined);
    if (target) link("Open record", item.name, target);
    const approval = item.approval_request_id;
    if (approval) row.dataset["approval"] = String(approval);
    if (item.status === "awaiting_approval" && approval) {
      link("View approval", "Approval", {res_model:"ir.approval.request", res_id:approval});
      addButton("Check approval", async button => {
        const scope = row.closest('[role="log"]');
        button.disabled = true;
        try {
          await chat.resumeOperation(approval);
          if (scope?.isConnected && document.activeElement === document.body) {
            scope.querySelector<HTMLButtonElement>('[data-approval="' + approval + '"] button')?.focus();
          }
        } catch (error) {
          label.textContent = error instanceof Error ? error.message : "Could not check approval.";
          button.disabled = false;
        }
      });
    }
    host.append(row);
  }
}

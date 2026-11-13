/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { ClientError } from "./transport.js";

/** One bounded modal per application. Diagnostics contain no request arguments or raw bodies. */
export class ErrorDialog {
  private modal: DialogHandle | undefined;
  constructor(private readonly dialogs?: DialogService) {}
  private dialog: HTMLDialogElement | undefined;
  private repeated = 0;
  private restore: HTMLElement | undefined;
  show(error: unknown): void {
    // Preserve the first failure rather than creating an unbounded stack of modals.
    if (this.dialog) {
      this.repeated = Math.min(this.repeated + 1, 999);
      const status = this.dialog.querySelector("[data-error-repeated]");
      if (status) status.textContent = `Additional failures: ${this.repeated}. Close this dialog before trying another operation.`;
      this.dialog.querySelector<HTMLButtonElement>("[data-error-close]")?.focus(); return;
    }
    this.restore = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
    const dialog = el("dialog", undefined, "choice-dialog message-dialog client-error-dialog");
    dialog.dataset["size"] = "md";
    this.dialog = dialog;
    const heading = el("h2", error instanceof ClientError && error.kind === "ambiguous" ? "Check whether your change was saved" : "Unable to complete the operation");
    heading.id = "client-error-heading";
    dialog.setAttribute("aria-labelledby", heading.id);
    dialog.setAttribute("aria-describedby", "client-error-message");
    const message = el("p", errorMessage(error).slice(0, 8192)); message.id = "client-error-message";
    const details = el("details"), summary = el("summary", "Technical details");
    const data = error instanceof ClientError ? error.diagnostics : undefined;
    const lines = [error instanceof Error ? error.name : "Application error", message.textContent ?? ""];
    // The id an administrator looks the call up by in the server log (scripts/erp-log.sh).
    const reference = data?.requestId ? el("p", undefined, "error-request-id") : undefined;
    if (reference && data?.requestId) reference.append("Error ID: ", el("code", data.requestId));
    if (data) {
      if (data.requestId) lines.push(`Error ID: ${data.requestId}`);
      lines.push(`Request: ${data.endpoint}`, `HTTP status: ${data.status}`);
      if (data.name) lines.push(`Server error: ${data.name}`);
      if (data.timestamp !== undefined) lines.push(`Server timestamp: ${data.timestamp}`);
      if (data.reason) lines.push(data.reason);
      if (data.debug) lines.push("", data.debug);
    }
    // JavaScript stacks are not exposed by a browser URL flag. Server debug is already gated.
    const diagnostic = lines.join("\n"), trace = el("pre", diagnostic);
    const copyStatus = el("p", undefined, "error-copy-status"); copyStatus.setAttribute("role", "status");
    const copy = button("Copy details", () => {
      void (async () => {
        try { await navigator.clipboard.writeText(diagnostic); copyStatus.textContent = "Details copied."; }
        catch { copyStatus.textContent = "Copy failed. Select and copy the details above."; }
      })();
    });
    details.append(summary, trace, copy, copyStatus);
    const close = button("Close", () => this.dispose(), "primary"); close.setAttribute("data-error-close", "");
    const repeated = el("p"); repeated.setAttribute("data-error-repeated", ""); repeated.setAttribute("role", "status");
    const actions = el("div", undefined, "actions"); actions.append(close);
    // The shared dialog anatomy (dialog-service.ts): header with the title and a close button, body, footer.
    const header = el("header", undefined, "message-dialog-header");
    const headerClose = button("×", () => this.dispose(), "message-dialog-close"); headerClose.setAttribute("aria-label", "Close");
    header.append(heading, headerClose);
    const body = el("div", undefined, "message-dialog-body"); body.append(message, ...(reference ? [reference] : []), details, repeated);
    const footer = el("footer", undefined, "message-dialog-footer"); footer.append(actions);
    dialog.append(header, body, footer);
    dialog.addEventListener("cancel", event => { event.preventDefault(); this.dispose(); });
    dialog.addEventListener("close", () => this.dispose());
    if (this.dialogs) this.modal = this.dialogs.attach(dialog, { dismiss: () => { this.dispose(); }, onClose: () => this.dispose() });
    else { document.body.append(dialog); dialog.showModal(); }
    close.focus();
  }
  dispose(): void {
    const dialog = this.dialog, prior = this.restore, modal = this.modal;
    this.modal = undefined;
    this.dialog = undefined; this.restore = undefined; this.repeated = 0;
    if (!dialog) return;
    if (modal) modal.dispose(); else { dialog.close(); dialog.remove(); }
    if (prior?.isConnected) prior.focus();
  }
}

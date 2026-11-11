/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage, labelled } from "./dom.js";
import { DialogService, messageOptions, messageSignal, plainMessageObject, type DialogHandle } from "./dialog-service.js";
import { ClientError } from "./transport.js";

export interface ExportDialogOptions {
  readonly dialogs: DialogService;
  readonly fields: readonly { name: string; label: string }[];
  readonly formats: readonly { key: string; label: string }[];
  readonly download: (fields: readonly string[], format: string) => Promise<void>;
  readonly notice: (message: string, options: { level: "danger" }) => void;
  readonly report?: (error: unknown) => void; readonly signal?: AbortSignal;
}
/** Field/format selection only. The trusted caller owns the authorized download implementation. */
export function exportDialog(options: ExportDialogOptions): DialogHandle {
  messageOptions(options, ["dialogs", "fields", "formats", "download", "notice", "report", "signal"], "Export dialog");
  if (!(options.dialogs instanceof DialogService)) throw new ClientError("validation", "Export dialogs must be a DialogService.");
  messageSignal(options.signal);
  const label = (value: unknown): boolean => typeof value === "string" && value.trim().length > 0 && new TextEncoder().encode(value).length <= 8192;
  if (!Array.isArray(options.fields) || options.fields.length > 256) throw new ClientError("validation", "Export fields must contain at most 256 unique named fields.");
  for (const field of options.fields) {
    if (!plainMessageObject(field) || typeof field.name !== "string" || field.name.length > 256 || !/^[a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*$/.test(field.name) || !label(field.label)) throw new ClientError("validation", "Export fields must contain at most 256 unique named fields.");
    messageOptions(field, ["name", "label"], "Export field");
  }
  if (new Set(options.fields.map(field => field.name)).size !== options.fields.length) throw new ClientError("validation", "Export fields must contain at most 256 unique named fields.");
  if (!Array.isArray(options.formats) || options.formats.length < 1 || options.formats.length > 16) throw new ClientError("validation", "Export formats must contain 1 to 16 unique formats.");
  for (const format of options.formats) {
    if (!plainMessageObject(format) || typeof format.key !== "string" || format.key.length > 64 || !/^[a-zA-Z0-9_-]+$/.test(format.key) || !label(format.label)) throw new ClientError("validation", "Export formats must contain 1 to 16 unique formats.");
    messageOptions(format, ["key", "label"], "Export format");
  }
  if (new Set(options.formats.map(format => format.key)).size !== options.formats.length) throw new ClientError("validation", "Export formats must contain 1 to 16 unique formats.");
  if (typeof options.download !== "function") throw new ClientError("validation", "An export download callback is required.");
  if (typeof options.notice !== "function") throw new ClientError("validation", "An export notice callback is required.");
  if (options.report !== undefined && typeof options.report !== "function") throw new ClientError("validation", "Export report must be a function.");
  const body = el("div", undefined, "export-dialog-body");
  const fields = el("fieldset"), legend = el("legend", "Fields to export"); fields.append(legend);
  const inputs = options.fields.map(field => { const input = el("input"); input.type = "checkbox"; input.value = field.name; fields.append(labelled(field.label, input)); return input; });
  const format = el("select"); options.formats.forEach(item => format.append(new Option(item.label, item.key)));
  const status = el("p"); status.setAttribute("role", "alert");
  body.append(fields, labelled("File format", format), status);
  let pending = false, dead = false;
  let handle: DialogHandle;
  const footer = el("div", undefined, "actions");
  const operation = button("Download", () => {
    if (pending || dead) return;
    const selection = inputs.filter(input => input.checked).map(input => input.value);
    if (!selection.length) { options.notice("Choose at least one field to export.", { level: "danger" }); return; }
    pending = true; status.textContent = ""; disable();
    void (async () => { await options.download(selection, format.value); })().catch(error => {
      if (dead) return;
      status.textContent = errorMessage(error); options.report?.(error);
    }).finally(() => { pending = false; if (!dead) disable(); });
  }, "primary");
  operation.setAttribute("data-export", "");
  const cancel = button("Close", () => { if (!pending) handle.close(); }); footer.append(operation, cancel);
  const disable = (): void => { inputs.forEach(input => { input.disabled = pending; }); format.disabled = pending; operation.disabled = pending; cancel.disabled = pending; handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-close")!.disabled = pending; handle.dialog.setAttribute("aria-busy", String(pending)); };
  handle = options.dialogs.open({ title: "Export records", body, footer, size: "md", dismiss: () => !pending, onClose: () => { dead = true; }, ...(options.signal ? { signal: options.signal } : {}) });
  disable(); return handle;
}

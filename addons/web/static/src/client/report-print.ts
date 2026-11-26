/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { icon } from "./icons.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { attachmentViewerFile, openDocumentViewer, reportViewerFile, type ViewerFile } from "./document-viewer.js";
import { saveFile } from "./file-save.js";
import { ClientError, type CallOptions, type RpcClient } from "./transport.js";

/** A report the Print menu offers for a model (`report.job.erp_print_reports`). */
export interface PrintableReport { readonly report: string; readonly name: string }

export interface PrintServices {
  readonly rpc: RpcClient;
  readonly dialogs: DialogService;
  /** Downloads a same-origin address and rejects when the server refuses it; the default fetches the file and saves it. */
  readonly download?: (url: string) => Promise<void>;
  /** Shows a printed document in the page; the document viewer when omitted. */
  readonly view?: (file: ViewerFile) => void;
  /** Receives failures the viewer also shows in place, for the client error log. */
  readonly report?: (error: unknown) => void;
}

const MODEL = "report.job";
const STATUS_FIELDS = ["state", "total", "processed", "failed", "failures", "warnings", "error", "attachment_id"];

function object(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new ClientError("server", `The server did not answer ${what} with an object.`);
  return value as Record<string, unknown>;
}

/** A path on this server only: the server builds it, the client still refuses anything else. */
function samePath(url: unknown): string {
  if (typeof url !== "string" || !url.startsWith("/") || url.startsWith("//")) throw new ClientError("server", "The server answered the print request with an address outside this server.");
  return url;
}

/** Fetch first, save after: a render that fails reaches the user as an error, not as a broken browser download. */
async function fetchAndSave(services: PrintServices, url: string): Promise<void> {
  if (services.download) return services.download(url);
  const { blob, fileName } = await services.rpc.getFile(url, "application/pdf");
  saveFile(blob, fileName);
}

/** Show a printed document in the document viewer (or the caller's own `view`). */
function showDocument(services: PrintServices, file: ViewerFile): void {
  if (services.view) return services.view(file);
  openDocumentViewer({ rpc: services.rpc, dialogs: services.dialogs, ...(services.report ? { report: services.report } : {}) }, [file]);
}

/**
 * Whether the server asks for the printed document to be shown in the page (`open: "preview"`, the system
 * parameter `base.report_print_preview`) or saved as a file (`"download"`, also when the answer names neither).
 */
function previewAsked(answer: Record<string, unknown>): boolean {
  const open = answer["open"];
  if (open === undefined || open === "download") return false;
  if (open === "preview") return true;
  throw new ClientError("server", "The server answered the print request with an unknown way to open the document.");
}

/** The reports offered for `model`; empty when the server has no report runtime yet. */
export async function loadPrintReports(rpc: RpcClient, model: string, options: CallOptions = {}): Promise<PrintableReport[]> {
  const answer = await rpc.call(MODEL, "erp_print_reports", { res_model: model }, options);
  if (!Array.isArray(answer)) throw new ClientError("server", "The server did not answer the report list with a list.");
  return answer.map(item => {
    const entry = object(item, "a report of the Print menu");
    if (typeof entry["report"] !== "string" || typeof entry["name"] !== "string") throw new ClientError("server", "A report of the Print menu has no name.");
    return { report: entry["report"], name: entry["name"] };
  });
}

/**
 * Print `report` for `ids`. A small request is saved as a file at once, or shown in the document viewer when the
 * server asks for a preview; a large one becomes a background print whose progress, failures and result the
 * returned dialog shows.
 */
export async function printRecords(services: PrintServices, report: PrintableReport, model: string, ids: readonly number[]): Promise<DialogHandle | undefined> {
  if (!ids.length) throw new ClientError("validation", "Select at least one record to print.");
  const answer = object(await services.rpc.call(MODEL, "erp_print", { report: report.report, res_model: model, res_ids: [...ids] }, { write: true }), "the print request");
  const preview = previewAsked(answer);
  if (answer["mode"] === "sync") {
    const url = samePath(answer["url"]);
    if (preview) showDocument(services, reportViewerFile(url, report.name)); else await fetchAndSave(services, url);
    return undefined;
  }
  const id = answer["id"], poll = answer["poll_ms"];
  if (answer["mode"] !== "job" || typeof id !== "number" || typeof poll !== "number") throw new ClientError("server", "The server answered the print request in an unknown form.");
  return printJobDialog(services, report.name, id, poll, preview);
}

function jsonList(value: unknown): unknown[] {
  if (typeof value !== "string" || !value) return [];
  const parsed: unknown = JSON.parse(value);
  return Array.isArray(parsed) ? parsed : [];
}

/**
 * The dialog of one background print: it reads the job row every `pollMs` until the print ends or the dialog
 * closes. The finished file can be opened in the document viewer or saved; `preview` makes Open the main action.
 */
export function printJobDialog(services: PrintServices, title: string, id: number, pollMs: number, preview = false): DialogHandle {
  const body = el("div", undefined, "print-job-body");
  const progress = el("progress", undefined, "print-job-progress");
  progress.setAttribute("aria-label", "Print progress");
  const status = el("p", "Waiting to start…", "print-job-status");
  status.setAttribute("role", "status");
  const problem = el("p", undefined, "print-job-error"); problem.setAttribute("role", "alert"); problem.hidden = true;
  const failures = el("ul", undefined, "print-job-failures"); failures.hidden = true;
  const warnings = el("ul", undefined, "print-job-warnings"); warnings.hidden = true;
  body.append(status, progress, problem, failures, warnings);
  const footer = el("div", undefined, "actions");
  const openBtn = button("Open", () => {}, preview ? "primary" : undefined);
  const downloadBtn = button("Download PDF", () => {}, preview ? undefined : "primary");
  openBtn.hidden = true; downloadBtn.hidden = true;
  let handle: DialogHandle | undefined;
  footer.append(...(preview ? [downloadBtn, openBtn] : [openBtn, downloadBtn]), button("Close", () => handle?.close()));
  let closed = false, timer: ReturnType<typeof setTimeout> | undefined;
  handle = services.dialogs.open({ title: `Print: ${title}`, body, footer, size: "md", onClose: () => { closed = true; if (timer) clearTimeout(timer); } });
  const list = (host: HTMLUListElement, items: string[], more = 0): void => {
    host.replaceChildren(...items.map(text => el("li", text)));
    if (more > 0) host.append(el("li", `and ${more} more`));
    host.hidden = items.length === 0;
  };
  const tick = async (): Promise<void> => {
    if (closed) return;
    try {
      const rows = await services.rpc.call(MODEL, "read", { ids: [id], fields: STATUS_FIELDS }, { background: true });
      if (closed) return;
      if (!Array.isArray(rows) || rows.length !== 1) throw new ClientError("server", "The print job is no longer available.");
      const row = object(rows[0], "the print job");
      const total = Number(row["total"]) || 0, processed = Number(row["processed"]) || 0, failed = Number(row["failed"]) || 0;
      progress.max = Math.max(total, 1); progress.value = processed;
      const listed = jsonList(row["failures"]).map(item => object(item, "a print failure"));
      list(failures, listed.map(item => `Record ${String(item["id"])}: ${String(item["error"])}`), failed - listed.length);
      list(warnings, jsonList(row["warnings"]).map(String));
      const state = row["state"];
      if (state === "done") {
        status.textContent = failed ? `Printed ${processed - failed} of ${total} documents; ${failed} could not be printed.` : `Printed ${total} documents.`;
        const attachment = Array.isArray(row["attachment_id"]) ? row["attachment_id"][0] : row["attachment_id"];
        if (typeof attachment !== "number") throw new ClientError("server", "The finished print job has no result.");
        downloadBtn.onclick = () => {
          problem.hidden = true;
          fetchAndSave(services, `/erp/content/${attachment}?download=true`).catch((error: unknown) => { problem.textContent = errorMessage(error); problem.hidden = false; });
        };
        const file = attachmentViewerFile({ id: attachment, name: `${title}.pdf`, mimetype: "application/pdf" });
        openBtn.onclick = () => {
          problem.hidden = true;
          try { if (file) showDocument(services, file); }
          catch (error) { problem.textContent = errorMessage(error); problem.hidden = false; }
        };
        openBtn.hidden = false; downloadBtn.hidden = false;
        // Ctrl+Enter presses the main action, now that it shows (a hidden button cannot be a dialog's default).
        (preview ? openBtn : downloadBtn).dataset["defaultAction"] = "";
        return;
      }
      if (state === "failed") {
        status.textContent = "The print failed.";
        problem.textContent = typeof row["error"] === "string" ? row["error"] : "The print failed.";
        problem.hidden = false;
        return;
      }
      status.textContent = state === "queued" ? "Waiting to start…" : `Printing: ${processed} of ${total} documents.`;
    } catch (error) {
      if (closed) return;
      status.textContent = "Progress is not available.";
      problem.textContent = errorMessage(error); problem.hidden = false;
      return;
    }
    timer = setTimeout(() => void tick(), pollMs);
  };
  void tick();
  return handle;
}

export interface PrintDropdown {
  /** The Print button and its menu. */
  readonly node: HTMLElement;
  readonly dispose: () => void;
}

/**
 * The Print button of a list or a form, before Actions as in the reference client: a printer icon and "Print", opening one
 * entry per report of `model`. The reports are read in the background at once (through `reports`, which a caller may share
 * between buttons of the same model) and the button stays hidden until they arrive; a model without a report has no Print
 * button, as in the reference client. `pick` prints with the chosen report. When that first read fails the button is
 * shown and opening it reads again; a failure of a read the user asked for is reported.
 */
export function printDropdown(rpc: RpcClient, model: string, pick: (report: PrintableReport) => void, report: (error: unknown) => void,
  reports: () => Promise<PrintableReport[]> = () => loadPrintReports(rpc, model, { background: true })): PrintDropdown {
  const node = el("div", undefined, "nav-dropdown print-dropdown");
  node.hidden = true;
  const toggle = button("Print", () => {}, "print-toggle-btn has-action-icon");
  toggle.prepend(icon("printer"));
  toggle.setAttribute("aria-haspopup", "menu");
  const menu = el("div", undefined, "nav-dropdown-menu print-menu");
  menu.setAttribute("aria-label", "Print");
  node.append(toggle, menu);
  let live = true;
  let state: "idle" | "loading" | "done" = "idle";
  const load = (asked: boolean): void => {
    if (state !== "idle") return;
    state = "loading";
    void reports().then(entries => {
      if (!live) return;
      menu.replaceChildren();
      for (const entry of entries) {
        const item = button(entry.name, () => { dropdown.close(true); pick(entry); }, "dropdown-item print-report-btn");
        item.setAttribute("role", "menuitem");
        item.setAttribute("aria-label", `Print ${entry.name}`);
        menu.append(item);
      }
      state = "done";
      if (!entries.length) { dropdown.close(false); node.hidden = true; return; }
      node.hidden = false;
      dropdown.refresh();
    }).catch((error: unknown) => {
      state = "idle";
      if (!live) return;
      // A Print button that may have reports is not hidden by a failed background read: it stays, and opening it reads
      // again and reports what fails then.
      node.hidden = false;
      if (asked) report(error);
    });
  };
  const dropdown = attachMenuDropdown({ trigger: toggle, menu, mode: "class-open", onOpen: () => load(true) });
  load(false);
  return { node, dispose: () => { live = false; dropdown.dispose(); } };
}

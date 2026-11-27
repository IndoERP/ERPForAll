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
export type ExportScope = "selected" | "filtered";
type ExportField = ExportDialogOptions["fields"][number];
interface RecordExportOptions extends Omit<ExportDialogOptions, "fields" | "download"> {
  readonly fields: () => Promise<readonly ExportField[]>;
  readonly selectedCount: number;
  readonly defaultFields: readonly string[];
  readonly download: (fields: readonly string[], format: string, scope: ExportScope) => Promise<void>;
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
  const fields = el("fieldset"), legend = el("legend", "Fields to export"); fields.append(legend);
  const inputs = options.fields.map(field => { const input = el("input"); input.type = "checkbox"; input.value = field.name; fields.append(labelled(field.label, input)); return input; });
  return downloadDialog(options, fields, () => inputs.filter(input => input.checked).map(input => input.value), options.download);
}

/** Record lists add field ordering and the visible record scopes to the download dialog. */
export function exportRecordDialog(options: RecordExportOptions): DialogHandle {
  const scope = el("fieldset", undefined, "record-export-scope");
  scope.append(el("legend", "Records to export"));
  const selected = el("input"); selected.type = "radio"; selected.name = "export_scope"; selected.value = "selected";
  const filtered = el("input"); filtered.type = "radio"; filtered.name = selected.name; filtered.value = "filtered";
  selected.disabled = !options.selectedCount; selected.checked = !!options.selectedCount; filtered.checked = !selected.checked;
  for (const [radio, text] of [[selected, `Selected records (${options.selectedCount})`], [filtered, "All records matching current filters"]] as const) {
    const label = el("label");
    label.append(radio, el("span", text));
    scope.append(label);
  }
  const picker = orderedFields();
  const handle = downloadDialog(options, picker.node, picker.selection,
    (fields, format) => options.download(fields, format, selected.checked ? "selected" : "filtered"), scope);
  const operation = handle.dialog.querySelector<HTMLButtonElement>("[data-export]")!;
  const load = (): void => {
    operation.disabled = true;
    picker.loading();
    void options.fields().then(fields => {
      if (!handle.dialog.isConnected) return;
      picker.fields(fields, options.defaultFields);
      operation.disabled = !fields.length;
    }).catch(error => {
      if (handle.dialog.isConnected) picker.failed(errorMessage(error), load);
    });
  };
  load();
  return handle;
}

function orderedFields(): {
  node: HTMLElement;
  selection: () => string[];
  fields: (fields: readonly ExportField[], defaults: readonly string[]) => void;
  loading: () => void;
  failed: (message: string, retry: () => void) => void;
} {
  const node = el("div");
  const status = el("p", undefined, "impex-field-status"); status.setAttribute("role", "status");
  const grid = el("div", undefined, "impex-picker");
  const availablePane = el("section", undefined, "impex-picker-pane");
  const orderedPane = el("section", undefined, "impex-picker-pane");
  const search = el("input"); search.type = "search";
  const available = el("ul", undefined, "impex-field-list"); available.setAttribute("aria-label", "Available fields");
  const chosen = el("ol", undefined, "impex-field-list"); chosen.setAttribute("aria-label", "Selected fields");
  const availableEmpty = el("p", undefined, "impex-field-status");
  const chosenEmpty = el("p", "Choose the fields to include in the file.", "impex-field-status");
  availablePane.append(el("h3", "Available fields"), labelled("Search fields", search), available, availableEmpty);
  orderedPane.append(el("h3", "Selected fields"), chosen, chosenEmpty);
  grid.append(availablePane, orderedPane);
  node.append(status, grid);
  let fields: readonly ExportField[] = [];
  const selection: ExportField[] = [];
  let dragged: ExportField | undefined;

  const label = (field: ExportField): HTMLElement => {
    const detail = el("span", field.label, "impex-field-name");
    if (field.label !== field.name) detail.append(el("small", field.name));
    return detail;
  };
  const updateAvailable = (): void => {
    const query = search.value.trim().toLowerCase();
    const matches = fields.filter(field => !selection.includes(field) && [field.name, field.label].some(value => value.toLowerCase().includes(query)));
    available.replaceChildren(...matches.map(field => {
      const entry = el("li");
      const add = button("", () => {
        const index = [...available.children].indexOf(entry);
        selection.push(field);
        update();
        const next = available.children[index] ?? available.lastElementChild;
        (next?.querySelector("button") ?? search).focus();
      }, "impex-add-field");
      add.setAttribute("aria-label", `Add ${field.label} (${field.name})`);
      add.dataset["field"] = field.name;
      const plus = el("span", "+"); plus.setAttribute("aria-hidden", "true");
      add.append(label(field), plus);
      entry.append(add);
      return entry;
    }));
    availableEmpty.hidden = !!matches.length;
    availableEmpty.textContent = query ? "No fields match your search." : "No more fields to add.";
  };
  const move = (from: number, to: number, direction: "up" | "down" = "up"): void => {
    if (from === to || from < 0 || to < 0 || to >= selection.length) return;
    selection.splice(to, 0, selection.splice(from, 1)[0]!);
    update();
    const row = chosen.children[to]!;
    (row.querySelector<HTMLButtonElement>(`[data-move="${direction}"]:not([disabled])`) ?? row.querySelector("button:not([disabled])"))?.focus();
    status.textContent = `${selection[to]!.label} moved to position ${to + 1}.`;
  };
  const update = (): void => {
    updateAvailable();
    chosenEmpty.hidden = !!selection.length;
    chosen.replaceChildren(...selection.map((field, index) => {
      const entry = el("li");
      entry.draggable = true;
      entry.addEventListener("dragstart", event => {
        if (entry.closest("fieldset")?.disabled || !event.dataTransfer) { event.preventDefault(); return; }
        dragged = field;
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData("text/plain", field.name);
      });
      entry.addEventListener("dragend", () => { dragged = undefined; });
      entry.addEventListener("dragover", event => { if (dragged && dragged !== field) event.preventDefault(); });
      entry.addEventListener("drop", event => {
        if (!dragged || entry.closest("fieldset")?.disabled) return;
        event.preventDefault();
        const from = selection.indexOf(dragged);
        dragged = undefined;
        move(from, index);
      });
      const up = button("↑", () => move(index, index - 1, "up"));
      const down = button("↓", () => move(index, index + 1, "down"));
      const remove = button("×", () => {
        selection.splice(index, 1);
        update();
        const next = chosen.children[index] ?? chosen.lastElementChild;
        (next?.querySelector<HTMLButtonElement>("[data-remove]") ?? search).focus();
        status.textContent = `${field.label} removed.`;
      });
      up.disabled = index === 0; down.disabled = index === selection.length - 1;
      up.dataset["move"] = "up"; down.dataset["move"] = "down"; remove.dataset["remove"] = field.name;
      const actionLabel = selection.some(other => other !== field && other.label === field.label) ? `${field.label} (${field.name})` : field.label;
      up.setAttribute("aria-label", `Move ${actionLabel} up`);
      down.setAttribute("aria-label", `Move ${actionLabel} down`);
      remove.setAttribute("aria-label", `Remove ${actionLabel}`);
      const actions = el("span", undefined, "impex-field-actions");
      actions.append(up, down, remove);
      entry.append(label(field), actions);
      return entry;
    }));
  };
  search.addEventListener("input", updateAvailable);
  return {
    node,
    selection: () => selection.map(field => field.name),
    fields: (result, defaults) => {
      fields = result;
      const named = new Map(fields.map(field => [field.name, field]));
      selection.splice(0, selection.length, ...[...new Set(defaults)].flatMap(name => {
        const field = named.get(name);
        return field ? [field] : [];
      }));
      status.textContent = ""; grid.hidden = false; update();
    },
    loading: () => { grid.hidden = true; status.textContent = "Loading fields…"; },
    failed: (message, retry) => { status.replaceChildren(document.createTextNode(`${message} `), button("Retry", retry)); },
  };
}

function downloadDialog(
  options: Pick<ExportDialogOptions, "dialogs" | "formats" | "notice" | "report" | "signal">,
  fieldControls: HTMLElement,
  selectedFields: () => readonly string[],
  download: ExportDialogOptions["download"],
  scope?: HTMLFieldSetElement,
): DialogHandle {
  const body = el("div", undefined, "export-dialog-body");
  const controls = el("fieldset", undefined, "export-dialog-controls");
  const format = el("select"); options.formats.forEach(item => format.append(new Option(item.label, item.key)));
  const status = el("p", undefined, "impex-error"); status.setAttribute("role", "alert");
  const formatControl = labelled("File format", format);
  if (scope) {
    const settings = el("div", undefined, "record-export-settings");
    settings.append(scope, formatControl);
    controls.append(settings, fieldControls);
  } else controls.append(fieldControls, formatControl);
  body.append(controls, status);
  let pending = false, dead = false;
  let handle: DialogHandle;
  const footer = el("div", undefined, "actions");
  const operation = button("Download", () => {
    if (pending || dead) return;
    const selection = selectedFields();
    if (!selection.length) { const msg = "Choose at least one field to export."; status.textContent = msg; options.notice(msg, { level: "danger" }); return; }
    pending = true; status.textContent = ""; disable();
    void (async () => { await download(selection, format.value); })().catch(error => {
      if (dead) return;
      status.textContent = errorMessage(error); options.report?.(error);
    }).finally(() => { pending = false; if (!dead) disable(); });
  }, "primary");
  operation.setAttribute("data-export", "");
  operation.dataset["defaultAction"] = "";
  const cancel = button("Close", () => { if (!pending) handle.close(); }); footer.append(cancel, operation);
  const disable = (): void => { controls.disabled = pending; operation.disabled = pending; cancel.disabled = pending; handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-close")!.disabled = pending; handle.dialog.setAttribute("aria-busy", String(pending)); };
  handle = options.dialogs.open({ title: "Export records", body, footer, size: scope ? "lg" : "md", dismiss: () => !pending, onClose: () => { dead = true; }, ...(options.signal ? { signal: options.signal } : {}) });
  if (scope) handle.dialog.classList.add("impex-export-dialog");
  disable(); return handle;
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { button, el, errorMessage, labelled } from "./dom.js";
import { saveFile } from "./file-save.js";
import { decodeImpexFields, decodeImportTemplates, impexFormat, type ImpexField, type ImpexFormat } from "./impex-formats.js";
import { ClientError, type RpcClient } from "./transport.js";

const MAX_IMPORT_BYTES = 10 * 1024 * 1024;
const UNMAPPED = "!";
const PREVIEW_COLUMNS = 12;
const PREVIEW_TEXT_LIMIT = 240;

interface ImportDialogOptions {
  readonly rpc: RpcClient;
  readonly model: string;
  readonly context: Record<string, unknown>;
  readonly csrf: string;
  readonly dialogs: DialogService;
  readonly templateFields: readonly string[];
  readonly imported: (count: number) => Promise<void>;
  readonly notice: (message: string) => void;
  readonly report: (error: unknown) => void;
}
interface Preview {
  readonly header: string[];
  readonly rows: string[][];
  readonly row_count: number;
  readonly sheet: string | null;
  readonly sheets: string[];
  readonly sheet_error?: string;
}
interface ImportMessage {
  readonly error: boolean;
  readonly text: string;
}

function decodePreview(raw: unknown): Preview {
  const invalid = (): never => { throw new ClientError("server", "The server returned an invalid file preview."); };
  if (!raw || typeof raw !== "object") return invalid();
  const { header, rows, row_count, sheet, sheets, sheet_error } = raw as Record<string, unknown>;
  const strings = (values: unknown): values is string[] => Array.isArray(values) && values.every(value => typeof value === "string");
  if (!strings(header) || !Array.isArray(rows) || rows.some(row => !strings(row) || row.length !== header.length)
      || typeof row_count !== "number" || !Number.isSafeInteger(row_count) || row_count < rows.length
      || !strings(sheets) || (sheet !== null && (typeof sheet !== "string" || !sheets.includes(sheet)))) return invalid();
  if (sheet_error !== undefined) {
    if (typeof sheet_error !== "string" || !sheet_error || sheet === null || sheets.length < 2 || header.length || rows.length || row_count) return invalid();
    return { header, rows, row_count, sheet, sheets, sheet_error };
  }
  if (!header.length) return invalid();
  return { header, rows, row_count, sheet, sheets };
}

/** Match exact technical names before labels. Only use unambiguous labels and assign a field once. */
export function matchImportFields(header: readonly string[], fields: readonly ImpexField[]): (string | null)[] {
  const exact = new Set(fields.map(field => field.name));
  const labels = new Map<string, string | null>();
  for (const field of fields) {
    const label = field.label.trim().toLowerCase();
    labels.set(label, labels.has(label) ? null : field.name);
  }
  const mapping = header.map(text => {
    const title = text.trim();
    return exact.has(title) ? title : labels.get(title.toLowerCase()) ?? null;
  });
  const assigned = new Set<string>();
  // Reserve exact names even when a competing label occurs earlier in the file.
  for (const useExact of [true, false]) {
    header.forEach((title, index) => {
      if (exact.has(title.trim()) !== useExact) return;
      const field = mapping[index];
      if (field && assigned.has(field)) mapping[index] = null;
      else if (field) assigned.add(field);
    });
  }
  return mapping;
}

/** Keep technical relation paths present in a file; the server checks each hop on Test and Import. */
function fieldsForHeader(fields: readonly ImpexField[], header: readonly string[]): ImpexField[] {
  const known = new Set(fields.map(field => field.name));
  const result = [...fields];
  for (const title of header) {
    const name = title.trim();
    const root = name.split("/")[0]!;
    if (!known.has(name) && known.has(root) && /^[a-z_][a-z0-9_]*(?:\/(?:\.id|[a-z_][a-z0-9_]*))+$/.test(name)) {
      known.add(name);
      result.push({ name, label: name, required: false });
    }
  }
  return result;
}

function decodeResult(raw: unknown, testing: boolean): { count: number | false; messages: ImportMessage[] } {
  const invalid = (): never => { throw new ClientError("server", "The server returned an invalid import result."); };
  if (!raw || typeof raw !== "object") return invalid();
  const result = raw as Record<string, unknown>;
  if (!Array.isArray(result["messages"])) return invalid();
  const received = result[testing ? "count" : "ids"];
  let count: number | false = false;
  if (received !== false) {
    if (testing) {
      if (typeof received !== "number" || !Number.isSafeInteger(received) || received < 0) return invalid();
      count = received;
    } else {
      if (!Array.isArray(received) || received.some(id => !Number.isSafeInteger(id) || id <= 0)) return invalid();
      count = received.length;
    }
  }
  return { count, messages: result["messages"].map(loadMessage) };
}

function messageRows(raw: unknown, offset: number): readonly [number, number] | undefined {
  if (!raw || typeof raw !== "object" || !("from" in raw) || typeof raw.from !== "number" || !Number.isSafeInteger(raw.from)) return undefined;
  const from = raw.from + offset;
  const to = "to" in raw && typeof raw.to === "number" && Number.isSafeInteger(raw.to) ? raw.to + offset : from;
  return [from, to];
}

function sampleText(value: string): string {
  return value.length > PREVIEW_TEXT_LIMIT ? `${value.slice(0, PREVIEW_TEXT_LIMIT)}…` : value;
}

function loadMessage(raw: unknown): ImportMessage {
  if (!raw || typeof raw !== "object") throw new ClientError("server", "The server returned an invalid import message.");
  const message = raw as Record<string, unknown>;
  if (typeof message["message"] !== "string" || (message["type"] !== "error" && message["type"] !== "warning")) {
    throw new ClientError("server", "The server returned an invalid import message.");
  }
  const rows = messageRows(message["file_rows"], 0) ?? messageRows(message["rows"], 2);
  const where = !rows ? "" : rows[0] === rows[1] ? `Row ${rows[0]} · ` : `Rows ${rows[0]}–${rows[1]} · `;
  const field = message["field_name"] ?? message["field"];
  return { error: message["type"] === "error", text: `${where}${typeof field === "string" ? `${field} · ` : ""}${message["message"]}` };
}

export function importDialog(options: ImportDialogOptions): DialogHandle {
  const body = el("div", undefined, "import-dialog-body");
  const controls = el("fieldset", undefined, "impex-import-controls");
  const templates = el("div", undefined, "impex-templates");
  const templateActions = el("div", undefined, "impex-template-actions");
  const templateStatus = el("p", undefined, "impex-field-status"); templateStatus.setAttribute("role", "status");
  templates.append(el("h3", "Import templates"), templateActions, templateStatus);
  const upload = el("div", undefined, "impex-upload");
  const input = el("input"); input.type = "file"; input.accept = ".csv,.xlsx,text/csv,application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"; input.name = "import_file";
  input.hidden = true;
  const filePicker = el("div", undefined, "impex-file-picker");
  const fileSummary = el("span", "No file selected", "impex-field-status"); fileSummary.setAttribute("role", "status");
  filePicker.append(button("Choose file", () => input.click()), fileSummary, input);
  upload.append(el("h3", "Upload a CSV or Excel file"), filePicker, el("p", "You can also drop a file here. Maximum file size is 10 MiB.", "muted"));
  const separator = el("select");
  for (const [label, value] of [["Comma", ","], ["Semicolon", ";"], ["Tab", "\t"]] as const) separator.append(new Option(label, value));
  const sheet = el("select");
  const separatorLabel = labelled("CSV delimiter", separator), sheetLabel = labelled("Worksheet", sheet);
  const fileOptions = el("div", undefined, "impex-file-settings");
  fileOptions.append(separatorLabel, sheetLabel);
  const preview = el("div", undefined, "impex-preview");
  const status = el("p", undefined, "import-dialog-status"); status.setAttribute("role", "status");
  const error = el("p", undefined, "impex-error"); error.setAttribute("role", "alert");
  const messages = el("ul", undefined, "impex-messages"); messages.hidden = true; messages.setAttribute("aria-label", "Import messages");
  const retry = button("Retry", () => { void loadFields(); }); retry.hidden = true;
  controls.append(templates, upload, fileOptions, preview);
  body.append(controls);
  const footer = el("div", undefined, "impex-import-footer");
  const feedback = el("div", undefined, "impex-import-feedback");
  feedback.append(status, error, messages, retry);
  const actions = el("div", undefined, "actions");
  const close = button("Close", () => handle.close());
  const test = button("Test import", () => { void run(true); }); test.dataset["testImport"] = "";
  const commit = button("Import", () => { void run(false); }, "primary"); commit.dataset["import"] = "";
  test.disabled = commit.disabled = true;
  test.dataset["defaultAction"] = "";
  actions.append(close, test, commit);
  footer.append(feedback, actions);

  let handle: DialogHandle;
  let pending: "read" | "write" | undefined;
  let dead = false, completed = false;
  let fields: ImpexField[] | undefined;
  let file: File | undefined, format: ImpexFormat | undefined, documentPreview: Preview | undefined;
  let mapping: (string | null | undefined)[] = [];
  const base = { model: options.model, context: options.context };
  const sync = (): void => {
    controls.disabled = !!pending || !fields || completed;
    close.disabled = pending === "write";
    handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-close")!.disabled = close.disabled;
    handle.dialog.setAttribute("aria-busy", String(!!pending));
    separatorLabel.hidden = format !== "csv";
    sheetLabel.hidden = !(documentPreview?.sheets.length || sheet.options.length);
    preview.hidden = !documentPreview;
    test.disabled = !!pending || completed || !documentPreview?.row_count || mapping.includes(undefined) || !mapping.some(Boolean);
    commit.disabled = test.disabled;
  };
  const show = (info = "", refusal = "", details: readonly ImportMessage[] = []): void => {
    status.textContent = info;
    error.textContent = refusal;
    messages.replaceChildren(...details.map(item => el("li", item.error ? item.text : `Warning · ${item.text}`, item.error ? "impex-error" : "impex-warning")));
    messages.hidden = !details.length;
  };
  const busy = async (work: () => Promise<void>, effect: "read" | "write" = "read"): Promise<void> => {
    if (pending || dead) return;
    pending = effect; sync();
    try { await work(); }
    catch (failure) {
      if (dead) return;
      if (effect === "write" && failure instanceof ClientError && failure.kind === "ambiguous") completed = true;
      show("", errorMessage(failure));
    } finally {
      pending = undefined;
      if (!dead) sync();
    }
  };
  const invalidate = (): void => { show(); sync(); };

  async function loadFields(): Promise<void> {
    retry.hidden = true;
    await busy(async () => {
      show("Loading import fields…");
      try { fields = decodeImpexFields(await options.rpc.call(options.model, "erp_impex_fields", { operation: "import", context: options.context })); }
      catch (failure) { retry.hidden = false; throw failure; }
      if (dead) return;
      show();
    });
  }
  async function loadTemplates(): Promise<void> {
    templateStatus.textContent = "Loading templates…";
    try {
      const available = decodeImportTemplates(await options.rpc.call(options.model, "get_import_templates", { context: options.context }));
      if (dead) return;
      templateStatus.textContent = "";
      templateActions.replaceChildren(...available.map(template => button(`Download ${template.label} template`, () => {
        void busy(async () => {
          const byName = new Map(fields?.map(field => [field.name, field]));
          const templateFields = [...new Set(options.templateFields)].flatMap(name => {
            const field = byName.get(name);
            return field ? [{ name: field.name, label: field.label }] : [];
          });
          const download = await options.rpc.postImpexFile("template", { ...base, format: template.format, fields: templateFields }, impexFormat(template.format).mime, options.csrf);
          if (!dead) saveFile(download.blob, download.fileName);
        });
      })));
      templates.hidden = !available.length;
    } catch (failure) {
      if (!dead) templateStatus.replaceChildren(document.createTextNode(`${errorMessage(failure)} `), button("Retry templates", () => { void loadTemplates(); }));
    }
  }

  const readFile = (): void => {
    if (!file || !format || pending || dead) return;
    const chosen = file;
    const settings = { ...base, format, ...(format === "csv" ? { separator: separator.value } : sheet.value ? { sheet: sheet.value } : {}) };
    documentPreview = undefined; mapping = []; invalidate(); preview.replaceChildren();
    void busy(async () => {
      show("Reading file…");
      const result = decodePreview(await options.rpc.postImport("preview", chosen, settings, options.csrf));
      if (dead) return;
      sheet.replaceChildren(...result.sheets.map(name => new Option(name, name)));
      if (result.sheet !== null) sheet.value = result.sheet;
      if (result.sheet_error) { show("", result.sheet_error); return; }
      documentPreview = result;
      const candidates = fieldsForHeader(fields ?? [], result.header);
      mapping = matchImportFields(result.header, candidates).map(field => field ?? undefined);
      renderPreview(result, candidates);
      if (result.row_count === 0) show("", "The file has no data rows below its column names.");
      else show();
    });
  };
  const choose = (chosen: File): void => {
    if (pending || !fields || completed) return;
    file = undefined; format = undefined; documentPreview = undefined; mapping = [];
    sheet.replaceChildren(); separator.value = ","; preview.replaceChildren();
    fileSummary.textContent = chosen.name;
    invalidate();
    const extension = /\.(csv|xlsx)$/i.exec(chosen.name)?.[1]?.toLowerCase();
    if (extension !== "csv" && extension !== "xlsx") { show("", "Choose a CSV or Excel (.xlsx) file."); return; }
    if (chosen.size > MAX_IMPORT_BYTES) { show("", "The file is larger than 10 MiB. Split it into smaller files."); return; }
    file = chosen; format = extension;
    readFile();
  };
  input.addEventListener("click", () => { input.value = ""; });
  input.addEventListener("change", () => {
    const chosen = input.files?.[0];
    if (chosen) choose(chosen);
  });
  separator.addEventListener("change", readFile);
  sheet.addEventListener("change", readFile);
  upload.addEventListener("dragover", event => {
    if (!event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    if (!controls.disabled) upload.dataset["over"] = "";
    event.dataTransfer.dropEffect = controls.disabled ? "none" : "copy";
  });
  upload.addEventListener("dragleave", event => {
    if (!(event.relatedTarget instanceof Node && upload.contains(event.relatedTarget))) delete upload.dataset["over"];
  });
  upload.addEventListener("drop", event => {
    event.preventDefault(); delete upload.dataset["over"];
    if (controls.disabled) return;
    const dropped = event.dataTransfer?.files;
    if (dropped?.length === 1) { input.value = ""; choose(dropped[0]!); }
    else if (dropped?.length) show("", "Drop one file at a time.");
  });

  function renderPreview(result: Preview, candidates: readonly ImpexField[]): void {
    const summary = el("p", `Showing ${result.rows.length} of ${result.row_count} data rows.`, "impex-field-status");
    const scroller = el("div", undefined, "impex-table-scroll");
    scroller.tabIndex = 0; scroller.setAttribute("role", "region"); scroller.setAttribute("aria-label", "File preview and column mapping");
    const table = el("table", undefined, "impex-preview-table");
    const navigation = el("div", undefined, "impex-preview-navigation");
    const range = el("span"); range.setAttribute("role", "status");
    let start = 0;
    const previous = button("Previous columns", () => { start -= PREVIEW_COLUMNS; draw(); if (previous.disabled) next.focus(); });
    const next = button("Next columns", () => { start += PREVIEW_COLUMNS; draw(); if (next.disabled) previous.focus(); });
    navigation.append(previous, range, next);
    navigation.hidden = result.header.length <= PREVIEW_COLUMNS;
    const mappingInfo = el("p", undefined, "impex-field-status"); mappingInfo.setAttribute("role", "status");
    function draw(): void {
      const end = Math.min(start + PREVIEW_COLUMNS, result.header.length);
      range.textContent = `Columns ${start + 1}–${end} of ${result.header.length}`;
      previous.disabled = start === 0; next.disabled = end === result.header.length;
      const head = el("thead");
      const titles = el("tr");
      const mapRow = el("tr");
      const selectors = result.header.slice(start, end).map((name, relative) => {
        const index = start + relative;
        const title = name.trim() ? sampleText(name) : `Column ${index + 1}`;
        const th = el("th", title); th.scope = "col";
        titles.append(th);
        const select = el("select");
        select.setAttribute("aria-label", `Field for column ${index + 1}: ${title}`);
        const unmapped = new Option("Choose a field", UNMAPPED); unmapped.disabled = true;
        select.append(unmapped, new Option("Ignore column", ""));
        for (const field of candidates) select.append(new Option(field.label === field.name ? field.label : `${field.label} (${field.name})`, field.name));
        select.value = mapping[index] === undefined ? UNMAPPED : mapping[index] ?? "";
        select.addEventListener("change", () => {
          if (pending || select.selectedOptions[0]?.disabled) {
            select.value = mapping[index] === undefined ? UNMAPPED : mapping[index] ?? "";
            return;
          }
          mapping[index] = select.value || null;
          updateMappings();
          invalidate();
        });
        const cell = el("td");
        cell.append(select); mapRow.append(cell);
        return select;
      });
      function updateMappings(): void {
        const values = new Set(mapping);
        for (const [index, select] of selectors.entries()) {
          for (const option of select.options) option.disabled = option.value === UNMAPPED || (!!option.value && option.value !== mapping[start + index] && values.has(option.value));
        }
        const unresolved = mapping.filter(value => value === undefined).length;
        const ignored = mapping.filter(value => value === null).length;
        mappingInfo.textContent = unresolved
          ? "Choose a field or Ignore column for each unmapped column."
          : ignored ? `${ignored} ${ignored === 1 ? "column is" : "columns are"} ignored.` : "";
      }
      updateMappings();
      head.append(titles, mapRow);
      const rows = el("tbody");
      for (const row of result.rows) {
        const line = el("tr"); line.append(...row.slice(start, end).map(value => el("td", sampleText(value)))); rows.append(line);
      }
      table.replaceChildren(head, rows); scroller.scrollLeft = 0;
    }
    draw(); scroller.append(table);
    preview.replaceChildren(el("h3", "Preview and map columns"), summary, navigation, scroller, mappingInfo);
  }

  async function run(testing: boolean): Promise<void> {
    if (!file || !format || !documentPreview || (testing ? test.disabled : commit.disabled)) return;
    const chosen = file;
    const settings = {
      ...base, format, fields: mapping.map(field => field ?? null),
      ...(format === "csv" ? { separator: separator.value } : documentPreview.sheet ? { sheet: documentPreview.sheet } : {}),
    };
    await busy(async () => {
      show(testing ? "Testing import…" : "Importing records…");
      const answer = await options.rpc.postImport(testing ? "test" : "import", chosen, settings, options.csrf);
      let result: ReturnType<typeof decodeResult>;
      try { result = decodeResult(answer, testing); }
      catch (failure) {
        if (testing) throw failure;
        throw new ClientError("ambiguous", "The server returned an unreadable answer. Check whether the records were imported before trying again.");
      }
      if (result.count === false || result.messages.some(message => message.error)) {
        if (!dead) show("", testing ? "The test found problems. No records were imported." : "Nothing was imported. Correct the errors and try again.", result.messages);
        return;
      }
      const records = `${result.count} ${result.count === 1 ? "record" : "records"}`;
      if (testing) {
        if (!dead) {
          show(`Test passed for ${records}. No records were imported.`, "", result.messages);
        }
        return;
      }
      completed = true;
      if (!dead) {
        if (result.messages.length) show(`${records} imported.`, "", result.messages);
        else handle.close();
      }
      options.notice(`${records} imported.`);
      try { await options.imported(result.count); }
      catch (failure) { options.report(failure); }
    }, testing ? "read" : "write");
  }

  handle = options.dialogs.open({ title: "Import records", body, footer, size: "lg", dismiss: () => pending !== "write", onClose: () => { dead = true; } });
  handle.dialog.classList.add("impex-import-dialog");
  sync(); void loadFields(); void loadTemplates();
  return handle;
}

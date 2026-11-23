/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage, labelled } from "./dom.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { exportDialog } from "./export-dialog.js";
import { saveFile } from "./file-save.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { ClientError, type RpcClient } from "./transport.js";

/** What the caller may do with a model's records as files (`erp_impex_rights`). */
export interface ImpexRights { readonly export: boolean; readonly import: boolean }

/** Most records one export writes; a larger search is refused with a sentence, never cut silently. */
export const MAX_EXPORT_RECORDS = 10_000;
/** Most data rows one import file may hold, and its largest size. */
export const MAX_IMPORT_ROWS = 10_000;
export const MAX_IMPORT_BYTES = 10 * 1024 * 1024;

export function decodeImpexRights(raw: unknown): ImpexRights {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new ClientError("server", "The server did not answer the import and export rights with an object.");
  const value = raw as Record<string, unknown>;
  if (Object.keys(value).length !== 2 || typeof value["export"] !== "boolean" || typeof value["import"] !== "boolean") throw new ClientError("server", "The server answered the import and export rights in an unknown form.");
  return { export: value["export"], import: value["import"] };
}

/**
 * One CSV document (RFC 4180, CRLF rows). A text cell that a spreadsheet could take for a formula (it starts with `=`, `+`,
 * `-`, `@`, a tab or a carriage return) is written with a leading apostrophe, the same rule as the table downloads.
 */
export function csvEncode(rows: readonly (readonly unknown[])[]): string {
  const cell = (value: unknown): string => {
    let text: string;
    if (value === null || value === undefined || value === false) text = "";
    else if (typeof value === "string") text = /^[=+\-@\t\r]/.test(value) ? `'${value}` : value;
    else if (typeof value === "number" || typeof value === "boolean") text = String(value);
    else throw new ClientError("server", "An exported cell is neither text, a number nor a truth value.");
    return /[",\r\n]/.test(text) || text !== text.trim() ? `"${text.replace(/"/g, "\"\"")}"` : text;
  };
  return rows.map(row => row.map(cell).join(",")).join("\r\n") + "\r\n";
}

/** Parse CSV text (RFC 4180, comma separated, `"` quoted, a leading byte order mark ignored); a broken quote is refused. */
export function csvDecode(text: string): string[][] {
  const source = text.startsWith("\ufeff") ? text.slice(1) : text;
  const rows: string[][] = [];
  let row: string[] = [], cell = "", quoted = false, i = 0, line = 1, quoteLine = 1;
  const endCell = (): void => { row.push(cell); cell = ""; };
  const endRow = (): void => { endCell(); rows.push(row); row = []; };
  while (i < source.length) {
    const char = source[i]!;
    if (quoted) {
      if (char === "\"") {
        if (source[i + 1] === "\"") { cell += "\""; i += 2; continue; }
        quoted = false; i += 1;
        const next = source[i];
        if (next !== undefined && next !== "," && next !== "\r" && next !== "\n") throw new ClientError("validation", `Line ${line} of the file has text after a closing quote.`);
        continue;
      }
      if (char === "\n") line += 1;
      cell += char; i += 1; continue;
    }
    if (char === "\"") {
      if (cell !== "") throw new ClientError("validation", `Line ${line} of the file has a quote inside an unquoted value.`);
      quoted = true; quoteLine = line; i += 1; continue;
    }
    if (char === ",") { endCell(); i += 1; continue; }
    if (char === "\r" || char === "\n") {
      endRow(); i += char === "\r" && source[i + 1] === "\n" ? 2 : 1; line += 1; continue;
    }
    cell += char; i += 1;
  }
  if (quoted) throw new ClientError("validation", `Line ${quoteLine} of the file opens a quote that is never closed.`);
  if (cell !== "" || row.length) endRow();
  // blank lines (one empty cell) carry no record
  return rows.filter(cells => !(cells.length === 1 && cells[0] === ""));
}

export interface ImportField { readonly name: string; readonly label: string }

/**
 * The `load` columns of a header row: each cell is a field name or path of the model (`partner_id`, `partner_id/id`) or the
 * label of one of `fields` (case and surrounding spaces ignored). An unknown or repeated column is refused by name.
 */
export function importColumns(header: readonly string[], fields: readonly ImportField[]): string[] {
  if (!header.length || header.every(cell => !cell.trim())) throw new ClientError("validation", "The first line of the file must name the columns.");
  const byLabel = new Map<string, string>();
  for (const field of fields) byLabel.set(field.label.trim().toLowerCase(), field.name);
  const names = new Set(fields.map(field => field.name));
  const unknown: string[] = [];
  const columns = header.map(raw => {
    const cell = raw.trim();
    const root = cell.split("/")[0]!;
    if (/^[a-z_][a-z0-9_]*(?:\/(?:id|\.id|[a-z_][a-z0-9_]*))*$/.test(cell) && names.has(root)) return cell;
    const named = byLabel.get(cell.toLowerCase());
    if (named) return named;
    unknown.push(cell || "(empty)");
    return "";
  });
  if (unknown.length) throw new ClientError("validation", `These columns are not fields of this list: ${unknown.join(", ")}.`);
  const seen = new Set<string>();
  for (const column of columns) {
    if (seen.has(column)) throw new ClientError("validation", `The column ${column} appears twice.`);
    seen.add(column);
  }
  return columns;
}

/** One message of a `load` answer, as a sentence with the file line it is about (the header is line 1). */
function loadMessage(raw: unknown): string {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new ClientError("server", "The server answered the import with an unknown message.");
  const message = raw as Record<string, unknown>;
  if (typeof message["message"] !== "string") throw new ClientError("server", "An import message has no text.");
  const rows = message["rows"] as Record<string, unknown> | undefined;
  const from = rows && typeof rows["from"] === "number" ? rows["from"] : undefined;
  const to = rows && typeof rows["to"] === "number" ? rows["to"] : undefined;
  const where = from === undefined ? "" : from === to || to === undefined ? `Line ${from + 2}: ` : `Lines ${from + 2} to ${to + 2}: `;
  return `${where}${message["message"]}`;
}

export interface ListDataMenuOptions {
  readonly rpc: RpcClient;
  readonly model: string;
  readonly context: Record<string, unknown>;
  readonly dialogs: DialogService;
  /** The list offers creating and changing records (its view's `create` and `edit`); Import also needs this. */
  readonly canImport: boolean;
  /** The columns Export offers and the labels Import recognises. */
  readonly fields: readonly ImportField[];
  /** The records Export writes: the selection, or every record of the current search. */
  readonly exportIds: () => Promise<number[]>;
  /** Runs after an import created or changed records. */
  readonly imported: (count: number) => Promise<void>;
  readonly notice: (message: string) => void;
  readonly report: (error: unknown) => void;
}

/**
 * The Import / Export menu of a list's control row. The rights are read when the menu is first opened (not on every list
 * open) and kept; the menu then offers Import only to a caller who may create and change the records and the list allows
 * it, and Export only to a caller with the export right. A failed read is reported and tried again at the next opening.
 */
export function listDataMenu(options: ListDataMenuOptions): { node: HTMLElement; dispose(): void } {
  const node = el("div", undefined, "nav-dropdown list-data-dropdown");
  const toggle = button("Import / Export", () => {}, "list-data-toggle-btn");
  toggle.setAttribute("aria-haspopup", "menu");
  const menu = el("div", undefined, "nav-dropdown-menu list-data-menu");
  menu.setAttribute("aria-label", "Import / Export");
  node.append(toggle, menu);
  let live = true, rights: ImpexRights | undefined, loading = false;
  let open: DialogHandle | undefined;
  const item = (label: string, action: () => void, className: string): HTMLButtonElement => {
    const entry = button(label, () => { dropdown.close(true); action(); }, `dropdown-item ${className}`);
    entry.setAttribute("role", "menuitem");
    return entry;
  };
  const fill = (granted: ImpexRights): void => {
    menu.replaceChildren();
    if (granted.import && options.canImport) menu.append(item("Import records", () => { open = importDialog(options); }, "list-import-btn"));
    if (granted.export) menu.append(item("Export", () => { open = openExport(options); }, "list-export-btn"));
    if (!menu.childElementCount) {
      const none = button("You may not import or export these records.", () => {}, "dropdown-item list-data-none");
      none.disabled = true; none.setAttribute("role", "menuitem");
      menu.append(none);
    }
    dropdown.refresh();
  };
  const load = (): void => {
    if (rights) { fill(rights); return; }
    if (loading) return;
    loading = true;
    void options.rpc.call(options.model, "erp_impex_rights", { context: options.context }).then(raw => {
      if (!live) return;
      rights = decodeImpexRights(raw);
      fill(rights);
    }).catch((error: unknown) => { if (live) { dropdown.close(false); options.report(error); } }).finally(() => { loading = false; });
  };
  const dropdown = attachMenuDropdown({ trigger: toggle, menu, mode: "class-open", onOpen: load });
  return { node, dispose: () => { live = false; open?.dispose(); dropdown.dispose(); } };
}

function openExport(options: ListDataMenuOptions): DialogHandle {
  return exportDialog({
    dialogs: options.dialogs,
    fields: options.fields,
    formats: [{ key: "csv", label: "CSV (.csv)" }],
    notice: message => options.notice(message),
    report: options.report,
    download: async fields => {
      const ids = await options.exportIds();
      if (!ids.length) throw new ClientError("validation", "There are no records to export.");
      const answer = await options.rpc.call(options.model, "export_data", { ids, fields_to_export: [...fields], context: { ...options.context, import_compat: false } });
      const datas = answer !== null && typeof answer === "object" && !Array.isArray(answer) ? (answer as Record<string, unknown>)["datas"] : undefined;
      if (!Array.isArray(datas) || datas.some(row => !Array.isArray(row) || row.length !== fields.length)) throw new ClientError("server", "The server answered the export in an unknown form.");
      const labels = fields.map(name => options.fields.find(field => field.name === name)?.label ?? name);
      saveFile(new Blob(["\ufeff", csvEncode([labels, ...(datas as unknown[][])])], { type: "text/csv;charset=utf-8" }), `${options.model}.csv`);
      options.notice(`${ids.length} ${ids.length === 1 ? "record" : "records"} exported.`);
    },
  });
}

function importDialog(options: ListDataMenuOptions): DialogHandle {
  const body = el("div", undefined, "import-dialog-body");
  const file = el("input"); file.type = "file"; file.accept = ".csv,text/csv"; file.name = "import_file";
  const hint = el("p", "A CSV file whose first line names the columns, by field label or field name.", "import-dialog-hint");
  const status = el("div", undefined, "import-dialog-status"); status.setAttribute("role", "alert");
  body.append(hint, labelled("File", file), status);
  const footer = el("div", undefined, "actions");
  let pending = false, handle!: DialogHandle;
  const show = (lines: readonly string[]): void => {
    status.replaceChildren(...lines.slice(0, 50).map(text => el("p", text)));
    if (lines.length > 50) status.append(el("p", `and ${lines.length - 50} more`));
  };
  const run = button("Import", () => {
    if (pending) return;
    const chosen = file.files?.[0];
    if (!chosen) { show(["Choose a CSV file to import."]); return; }
    pending = true; run.disabled = true; show([]);
    void (async () => {
      if (chosen.size > MAX_IMPORT_BYTES) throw new ClientError("validation", "The file is larger than 10 MB. Split it into smaller files.");
      const rows = csvDecode(await chosen.text());
      const [header, ...data] = rows;
      if (!header) throw new ClientError("validation", "The file is empty.");
      if (!data.length) throw new ClientError("validation", "The file has no rows below its column names.");
      if (data.length > MAX_IMPORT_ROWS) throw new ClientError("validation", `The file has ${data.length} rows; at most ${MAX_IMPORT_ROWS} are imported at once.`);
      const fields = importColumns(header, options.fields);
      data.forEach((row, index) => { if (row.length !== fields.length) throw new ClientError("validation", `Line ${index + 2} has ${row.length} values; the first line names ${fields.length} columns.`); });
      const answer = await options.rpc.call(options.model, "load", { fields, data, context: options.context }, { write: true });
      if (answer === null || typeof answer !== "object" || Array.isArray(answer)) throw new ClientError("server", "The server answered the import in an unknown form.");
      const result = answer as Record<string, unknown>;
      const messages = Array.isArray(result["messages"]) ? result["messages"].map(loadMessage) : undefined;
      if (!messages) throw new ClientError("server", "The server answered the import without its messages.");
      const ids = result["ids"];
      if (!Array.isArray(ids)) { show(["Nothing was imported. Correct the file and try again:", ...messages]); return; }
      handle.close();
      options.notice(`${ids.length} ${ids.length === 1 ? "record" : "records"} imported.`);
      await options.imported(ids.length);
    })().catch((error: unknown) => {
      show([errorMessage(error)]);
      if (!(error instanceof ClientError && error.kind === "validation")) options.report(error);
    }).finally(() => { pending = false; run.disabled = false; });
  }, "primary");
  run.setAttribute("data-import", "");
  footer.append(run, button("Close", () => { if (!pending) handle.close(); }));
  handle = options.dialogs.open({ title: "Import records", body, footer, size: "md", dismiss: () => !pending });
  return handle;
}

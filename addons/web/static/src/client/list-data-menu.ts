/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { exportRecordDialog } from "./export-dialog.js";
import { decodeImpexFields, IMPEX_FORMATS, impexFormat } from "./impex-formats.js";
import { importDialog } from "./import-dialog.js";
import { saveFile } from "./file-save.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { ClientError, type RpcClient } from "./transport.js";

/** What the caller may do with a model's records as files (`erp_impex_rights`). */
export interface ImpexRights { readonly export: boolean; readonly import: boolean }

/** Most records one export writes; a larger search is refused with a sentence, never cut silently. */
export const MAX_EXPORT_RECORDS = 10_000;

export function decodeImpexRights(raw: unknown): ImpexRights {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new ClientError("server", "The server did not answer the import and export rights with an object.");
  const value = raw as Record<string, unknown>;
  if (Object.keys(value).length !== 2 || typeof value["export"] !== "boolean" || typeof value["import"] !== "boolean") throw new ClientError("server", "The server answered the import and export rights in an unknown form.");
  return { export: value["export"], import: value["import"] };
}

export interface ListDataMenuOptions {
  readonly rpc: RpcClient;
  readonly model: string;
  readonly context: Record<string, unknown>;
  readonly csrf: string;
  readonly dialogs: DialogService;
  /** The list offers creating and changing records (its view's `create` and `edit`); Import also needs this. */
  readonly canImport: boolean;
  /** Current list columns, in display order. */
  readonly visibleFields: () => readonly string[];
  readonly selectedIds: () => readonly number[];
  readonly filteredIds: () => Promise<number[]>;
  /** Runs after an import created or changed records. */
  readonly imported: (count: number) => Promise<void>;
  readonly notice: (message: string, options?: { level?: "danger" }) => void;
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
    if (granted.import && options.canImport) menu.append(item("Import records", () => {
      open = importDialog({ ...options, templateFields: options.visibleFields() });
    }, "list-import-btn"));
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
  const selected = [...options.selectedIds()];
  let catalogue: ReturnType<typeof decodeImpexFields> = [];
  return exportRecordDialog({
    dialogs: options.dialogs,
    fields: async () => {
      catalogue = decodeImpexFields(await options.rpc.call(options.model, "erp_impex_fields", { operation: "export", context: options.context }));
      return catalogue;
    },
    selectedCount: selected.length,
    defaultFields: options.visibleFields(),
    formats: IMPEX_FORMATS,
    notice: options.notice,
    report: options.report,
    download: async (fields, type, scope) => {
      const ids = scope === "selected" ? selected : await options.filteredIds();
      if (!ids.length) throw new ClientError("validation", "There are no records to export.");
      if (ids.length > MAX_EXPORT_RECORDS) throw new ClientError("validation", "Select at most 10,000 records to export.");
      const labels = new Map(catalogue.map(field => [field.name, field.label]));
      const format = impexFormat(type);
      const { blob, fileName } = await options.rpc.postImpexFile("export", {
        model: options.model, format: format.key, context: options.context, ids,
        fields: fields.map(name => ({ name, label: labels.get(name)! })),
      }, format.mime, options.csrf);
      saveFile(blob, fileName);
      options.notice(`${ids.length} ${ids.length === 1 ? "record" : "records"} exported.`);
    },
  });
}

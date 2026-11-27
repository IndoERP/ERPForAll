/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { saveFile } from "../src/client/file-save.js";
import { decodeImpexRights, listDataMenu, type ListDataMenuOptions } from "../src/client/list-data-menu.js";
import { matchImportFields } from "../src/client/import-dialog.js";
import type { RpcClient } from "../src/client/transport.js";

vi.mock("../src/client/file-save.js", () => ({ saveFile: vi.fn() }));

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.clearAllMocks(); vi.restoreAllMocks(); document.body.replaceChildren(); });

interface Call { model: string; method: string; args: Record<string, unknown>; options: unknown }
interface FileCall { action: string; file?: File; document: Record<string, unknown>; csrf: string; type?: string }
function fakeRpc(answer: (call: Call) => unknown, upload?: (call: FileCall) => unknown): { rpc: RpcClient; calls: Call[]; fileCalls: FileCall[] } {
  const calls: Call[] = [], fileCalls: FileCall[] = [];
  const rpc = {
    call: vi.fn(async (model: string, method: string, args: Record<string, unknown>, options?: unknown) => {
      const call = { model, method, args, options }; calls.push(call); return answer(call);
    }),
    postImport: vi.fn(async (action: string, file: File, document: Record<string, unknown>, csrf: string) => {
      const call = { action, file, document, csrf }; fileCalls.push(call); return upload?.(call);
    }),
    postImpexFile: vi.fn(async (action: string, document: Record<string, unknown>, type: string, csrf: string) => {
      const call = { action, document, csrf, type }; fileCalls.push(call);
      return { blob: new Blob([], { type }), fileName: `notes.${document["format"]}` };
    }),
  };
  return { rpc: rpc as unknown as RpcClient, calls, fileCalls };
}

const fields = [
  { name: "name", label: "Name", required: true },
  { name: "partner_id", label: "Customer", required: false },
  { name: "partner_id/id", label: "Customer / External ID", required: false },
];

function menu(rpc: RpcClient, extra: Partial<ListDataMenuOptions> = {}): { node: HTMLElement; dispose(): void; options: ListDataMenuOptions } {
  const options: ListDataMenuOptions = {
    rpc, model: "test.note", context: { lang: "en_US" }, csrf: "test-token", dialogs, canImport: true,
    visibleFields: () => [], selectedIds: () => [5], filteredIds: vi.fn(async () => [5, 6]),
    imported: vi.fn(async () => {}), notice: vi.fn(), report: vi.fn(), ...extra,
  };
  const built = listDataMenu(options);
  document.body.append(built.node);
  return { ...built, options };
}

const open = (node: HTMLElement): void => node.querySelector<HTMLButtonElement>(".list-data-toggle-btn")!.click();
const entries = (node: HTMLElement): string[] => [...node.querySelectorAll(".list-data-menu button")].map(item => item.textContent ?? "");

test("import guesses unique labels and technical names, reserving exact names first", () => {
  const repeatedLabels = [...fields, { name: "customer_reference", label: "Customer", required: false }];
  expect(matchImportFields([" Name ", "Customer", "partner_id/id"], fields)).toEqual(["name", "partner_id", "partner_id/id"]);
  expect(matchImportFields(["Name", "name", "Customer", "partner_id", "?", ""], repeatedLabels)).toEqual([null, "name", null, "partner_id", null, null]);
  expect(matchImportFields(["name", "name"], fields)).toEqual(["name", null]);
});

test("the rights answer is decoded strictly", () => {
  expect(decodeImpexRights({ export: true, import: false })).toEqual({ export: true, import: false });
  expect(() => decodeImpexRights([])).toThrow("with an object");
  expect(() => decodeImpexRights({ export: true })).toThrow("unknown form");
  expect(() => decodeImpexRights({ export: "yes", import: false })).toThrow("unknown form");
});

test("the rights are read at the first opening only, and each entry follows its right", async () => {
  const { rpc, calls } = fakeRpc(() => ({ export: true, import: true }));
  const { node } = menu(rpc);
  expect(calls).toHaveLength(0); // opening a list asks nothing
  open(node);
  await vi.waitFor(() => expect(entries(node)).toEqual(["Import records", "Export"]));
  expect(calls).toEqual([{ model: "test.note", method: "erp_impex_rights", args: { context: { lang: "en_US" } }, options: undefined }]);
  open(node); open(node);
  expect(calls).toHaveLength(1);
});

test.each<[{ export: boolean; import: boolean }, boolean, string[]]>([
  [{ export: true, import: false }, true, ["Export"]],
  [{ export: false, import: true }, true, ["Import records"]],
  [{ export: true, import: true }, false, ["Export"]],
  [{ export: false, import: false }, true, ["You may not import or export these records."]],
])("rights %j with a list that %s allows importing offer %j", async (rights, canImport, expected) => {
  const { rpc } = fakeRpc(() => rights);
  const { node } = menu(rpc, { canImport });
  open(node);
  await vi.waitFor(() => expect(entries(node)).toEqual(expected));
});

test("a failed rights read is reported and read again at the next opening", async () => {
  let fail = true;
  const { rpc, calls } = fakeRpc(() => { if (fail) throw new Error("offline"); return { export: true, import: false }; });
  const { node, options } = menu(rpc);
  open(node);
  await vi.waitFor(() => expect(options.report).toHaveBeenCalledTimes(1));
  fail = false;
  open(node);
  await vi.waitFor(() => expect(entries(node)).toEqual(["Export"]));
  expect(calls).toHaveLength(2);
});

async function openEntry(node: HTMLElement, mode: "import" | "export"): Promise<HTMLDialogElement> {
  open(node);
  const selector = `.list-${mode}-btn`;
  await vi.waitFor(() => expect(node.querySelector(selector)).not.toBeNull());
  node.querySelector<HTMLButtonElement>(selector)!.click();
  return document.querySelector<HTMLDialogElement>("dialog[open]")!;
}

test("Export searches technical names and writes the reordered fields in XLSX for the current selection", async () => {
  const { rpc, fileCalls, calls } = fakeRpc(({ method }) => method === "erp_impex_rights" ? { export: true, import: false } : fields.slice(0, 2));
  const { node, options } = menu(rpc);
  const dialog = await openEntry(node, "export");
  await vi.waitFor(() => expect(dialog.querySelector("[data-field='name']")).not.toBeNull());
  const search = dialog.querySelector<HTMLInputElement>("input[type=search]")!;
  search.value = "PARTNER_ID";
  search.dispatchEvent(new Event("input"));
  expect(dialog.querySelector("[data-field='name']")).toBeNull();
  dialog.querySelector<HTMLButtonElement>("[data-field='partner_id']")!.click();
  search.value = ""; search.dispatchEvent(new Event("input"));
  dialog.querySelector<HTMLButtonElement>("[data-field='name']")!.click();
  dialog.querySelector<HTMLButtonElement>("[aria-label='Move Name up']")!.click();
  dialog.querySelector<HTMLButtonElement>("[data-export]")!.click();
  await vi.waitFor(() => expect(saveFile).toHaveBeenCalledTimes(1));
  expect(calls[1]).toMatchObject({ method: "erp_impex_fields", args: { operation: "export" } });
  expect(fileCalls[0]).toEqual({
    action: "export", document: { model: "test.note", ids: [5], format: "xlsx", context: { lang: "en_US" },
      fields: [{ name: "name", label: "Name" }, { name: "partner_id", label: "Customer" }] },
    type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", csrf: "test-token",
  });
  expect(vi.mocked(saveFile).mock.calls[0]?.[1]).toBe("notes.xlsx");
  expect(options.filteredIds).not.toHaveBeenCalled();
  expect(options.notice).toHaveBeenCalledWith("1 record exported.");
});

test("Export can explicitly choose filtered records and CSV even when records are selected", async () => {
  const { rpc, fileCalls } = fakeRpc(({ method }) => method === "erp_impex_rights" ? { export: true, import: false } : fields.slice(0, 2));
  const { node, options } = menu(rpc);
  const dialog = await openEntry(node, "export");
  await vi.waitFor(() => expect(dialog.querySelector("[data-field='name']")).not.toBeNull());
  dialog.querySelector<HTMLInputElement>("[value='filtered']")!.click();
  dialog.querySelector("select")!.value = "csv";
  dialog.querySelector<HTMLButtonElement>("[data-field='name']")!.click();
  dialog.querySelector<HTMLButtonElement>("[data-export]")!.click();
  await vi.waitFor(() => expect(saveFile).toHaveBeenCalledTimes(1));
  expect(options.filteredIds).toHaveBeenCalledTimes(1);
  expect(fileCalls[0]).toMatchObject({ action: "export", document: { ids: [5, 6], format: "csv" }, type: "text/csv" });
});

const preview = { header: ["Name", "Customer"], rows: [["First", "Alice"]], row_count: 1, sheet: null, sheets: [] };
const testSuccess = { count: 1, messages: [], nextrow: 0 };
const importSuccess = { ids: [7], messages: [], nextrow: 0 };
const testButton = (dialog: HTMLDialogElement): HTMLButtonElement => dialog.querySelector<HTMLButtonElement>("[data-test-import]")!;
const importButton = (dialog: HTMLDialogElement): HTMLButtonElement => dialog.querySelector<HTMLButtonElement>("[data-import]")!;
const mappings = (dialog: HTMLDialogElement): HTMLSelectElement[] => [...dialog.querySelectorAll<HTMLSelectElement>(".impex-preview-table select")];
const change = (input: HTMLInputElement | HTMLSelectElement, value: string): void => {
  input.value = value; input.dispatchEvent(new Event("change"));
};

function chooseFile(dialog: HTMLDialogElement, name: string): File {
  const input = dialog.querySelector<HTMLInputElement>("input[type='file']")!;
  const file = new File(["content"], name);
  Object.defineProperty(input, "files", { configurable: true, value: [file] });
  input.dispatchEvent(new Event("change"));
  return file;
}

async function startImport(upload = ({ action }: FileCall): unknown => {
  if (action === "preview") return preview;
  return action === "test" ? testSuccess : importSuccess;
}) {
  const { rpc, fileCalls } = fakeRpc(({ method }) => {
    if (method === "erp_impex_rights") return { export: false, import: true };
    if (method === "erp_impex_fields") return fields;
    return [{ format: "xlsx", label: "Excel (.xlsx)" }, { format: "csv", label: "CSV (.csv)" }];
  }, upload);
  const { node, options } = menu(rpc, { visibleFields: () => ["name", "unknown", "name"] });
  const dialog = await openEntry(node, "import");
  await vi.waitFor(() => expect(dialog.querySelector<HTMLFieldSetElement>("fieldset")!.disabled).toBe(false));
  return { dialog, fileCalls, options };
}

test("Import downloads both templates and sends only visible, importable template fields", async () => {
  const { dialog, fileCalls } = await startImport();
  const buttons = [...dialog.querySelectorAll<HTMLButtonElement>(".impex-template-actions button")];
  expect(buttons.map(button => button.textContent)).toEqual(["Download Excel (.xlsx) template", "Download CSV (.csv) template"]);
  for (const [index, button] of buttons.entries()) {
    button.click();
    await vi.waitFor(() => expect(saveFile).toHaveBeenCalledTimes(index + 1));
  }
  expect(fileCalls.map(call => call.action)).toEqual(["template", "template"]);
  expect(fileCalls.map(call => call.document["format"])).toEqual(["xlsx", "csv"]);
  expect(fileCalls[0]!.document["fields"]).toEqual([{ name: "name", label: "Name" }]);
});

test("Import previews, tests without a write, then imports the new mapping and reloads the list", async () => {
  const { dialog, fileCalls, options } = await startImport(({ action }) => action === "preview"
    ? { ...preview, header: [...preview.header, "Unmapped"], rows: [["First", "Alice", "X"]] }
    : action === "test" ? testSuccess : importSuccess);
  expect(importButton(dialog).disabled).toBe(true);
  const file = chooseFile(dialog, "notes.csv");
  await vi.waitFor(() => expect(mappings(dialog)).toHaveLength(3));
  expect(dialog.querySelector<HTMLInputElement>("input[type='file']")!.hidden).toBe(true);
  expect(dialog.querySelector(".impex-file-picker [role='status']")?.textContent).toBe("notes.csv");
  expect(dialog.querySelector(".impex-preview")?.textContent).toContain("First");
  expect(mappings(dialog).slice(0, 2).map(select => select.value)).toEqual(["name", "partner_id"]);
  expect(mappings(dialog)[1]!.querySelector<HTMLOptionElement>("option[value='name']")!.disabled).toBe(true);
  expect(importButton(dialog).disabled).toBe(true);
  change(mappings(dialog)[2]!, "");
  expect(importButton(dialog).disabled).toBe(false);
  testButton(dialog).click();
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
  expect(dialog.querySelector(".message-dialog-footer .import-dialog-status")?.textContent).toBe("Test passed for 1 record. No records were imported.");
  expect(fileCalls.map(call => call.action)).toEqual(["preview", "test"]);
  expect(options.imported).not.toHaveBeenCalled();

  change(mappings(dialog)[1]!, "partner_id/id");
  expect(dialog.querySelector(".import-dialog-status")?.textContent).toBe("");
  testButton(dialog).click();
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
  importButton(dialog).click();
  await vi.waitFor(() => expect(options.imported).toHaveBeenCalledWith(1));
  expect(fileCalls.map(call => call.action)).toEqual(["preview", "test", "test", "import"]);
  expect(fileCalls[3]).toMatchObject({
    file, document: { model: "test.note", format: "csv", fields: ["name", "partner_id/id", null], separator: "," },
  });
  expect(options.notice).toHaveBeenCalledWith("1 record imported.");
  expect(dialog.open).toBe(false);
});

test("Import uses the chosen Excel worksheet and discards the prior test on worksheet change", async () => {
  const { dialog, fileCalls } = await startImport(({ action, document }) => {
    if (action === "test") return testSuccess;
    return { ...preview, sheet: document["sheet"] ?? "Data", sheets: ["Data", "Archive"] };
  });
  const file = new File(["content"], "notes.XLSX");
  const drop = new Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [file] } });
  dialog.querySelector(".impex-upload")!.dispatchEvent(drop);
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  expect(dialog.querySelector(".impex-file-picker [role='status']")?.textContent).toBe("notes.XLSX");
  testButton(dialog).click();
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
  expect(fileCalls[1]).toMatchObject({ action: "test", document: { format: "xlsx", sheet: "Data" } });
  const worksheet = [...dialog.querySelectorAll<HTMLSelectElement>(".impex-file-settings select")][1]!;
  change(worksheet, "Archive");
  expect(importButton(dialog).disabled).toBe(true);
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  expect(dialog.querySelector(".import-dialog-status")?.textContent).toBe("");
  expect(fileCalls[2]).toMatchObject({ action: "preview", document: { format: "xlsx", sheet: "Archive" } });
});

test("A refused test renders the row and field message", async () => {
  const { dialog, options } = await startImport(({ action }) => action === "preview" ? preview : {
    count: false, messages: [{ type: "error", rows: { from: 1, to: 1 }, file_rows: { from: 5, to: 7 }, field: "name", field_name: "Name", message: "A value is required." }, { type: "warning", rows: { from: 0, to: 0 }, message: "Check this value." }],
  });
  chooseFile(dialog, "notes.csv");
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  testButton(dialog).click();
  await vi.waitFor(() => expect(dialog.querySelector(".impex-messages")!.textContent).toContain("Rows 5–7 · Name · A value is required."));
  expect(dialog.querySelector(".impex-messages")!.textContent).toContain("Row 2 · Check this value.");
  expect(dialog.querySelector(".message-dialog-footer [role='alert']")!.textContent).toContain("No records were imported.");
  expect(options.imported).not.toHaveBeenCalled();
});

test("dropping multiple files invalidates the previous preview before another import can run", async () => {
  const { dialog, fileCalls } = await startImport();
  chooseFile(dialog, "previous.csv");
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
  const drop = new Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File([], "first.csv"), new File([], "second.csv")] } });
  dialog.querySelector(".impex-upload")!.dispatchEvent(drop);
  expect(dialog.querySelector('[role="alert"]')?.textContent).toContain("Drop one file at a time.");
  expect(importButton(dialog).disabled).toBe(true);
  expect(testButton(dialog).disabled).toBe(true);
  expect(mappings(dialog)).toHaveLength(0);
  importButton(dialog).click();
  expect(fileCalls.some(request => request.action === "import")).toBe(false);
  chooseFile(dialog, "replacement.csv");
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
});

test("Re-selecting the file clears the previous test, and import can run directly", async () => {
  const { dialog, options } = await startImport();
  const reloadFailure = new Error("Reload failed");
  vi.mocked(options.imported).mockRejectedValueOnce(reloadFailure);
  chooseFile(dialog, "notes.csv");
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  testButton(dialog).click();
  await vi.waitFor(() => expect(importButton(dialog).disabled).toBe(false));
  chooseFile(dialog, "updated-notes.csv");
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  expect(dialog.querySelector(".import-dialog-status")?.textContent).toBe("");
  importButton(dialog).click();
  await vi.waitFor(() => expect(options.report).toHaveBeenCalledExactlyOnceWith(reloadFailure));
  expect(dialog.open).toBe(false);
});

test("Export starts with current visible columns in list order, intersected with the authorized fields", async () => {
  const { rpc } = fakeRpc(({ method }) => method === "erp_impex_rights" ? { export: true, import: false } : fields.slice(0, 2));
  const { node } = menu(rpc, { visibleFields: () => ["partner_id", "hidden_field", "name", "partner_id"] });
  const dialog = await openEntry(node, "export");
  const selected = () => [...dialog.querySelectorAll<HTMLButtonElement>("[data-remove]")].map(button => button.dataset["remove"]);
  await vi.waitFor(() => expect(selected()).toEqual(["partner_id", "name"]));
  dialog.querySelector<HTMLButtonElement>("[data-remove='name']")!.click();
  expect(selected()).toEqual(["partner_id"]);
  expect(dialog.querySelector("[data-field='name']")).not.toBeNull();
});

test("An unusable first worksheet keeps the other worksheets available for preview", async () => {
  const { dialog, fileCalls } = await startImport(({ action, document }) => {
    if (action !== "preview") return testSuccess;
    const sheets = ["Notes", "Data"];
    if (document["sheet"] === "Data") return { ...preview, sheet: "Data", sheets };
    return { header: [], rows: [], row_count: 0, sheet: "Notes", sheets, sheet_error: "The Notes sheet contains no column names." };
  });
  chooseFile(dialog, "notes.xlsx");
  await vi.waitFor(() => expect(dialog.querySelector("[role='alert']")?.textContent).toContain("The Notes sheet contains no column names."));
  const worksheet = [...dialog.querySelectorAll<HTMLSelectElement>(".impex-file-settings select")][1]!;
  expect(worksheet.value).toBe("Notes");
  expect(testButton(dialog).disabled).toBe(true);
  change(worksheet, "Data");
  await vi.waitFor(() => expect(testButton(dialog).disabled).toBe(false));
  expect(fileCalls[1]).toMatchObject({ action: "preview", document: { sheet: "Data" } });
});

test("A wide preview pages through columns without discarding earlier mappings", async () => {
  const header = ["Name", "Customer", ...Array.from({ length: 12 }, (_, index) => `Extra ${index + 1}`)];
  const { dialog, fileCalls } = await startImport(({ action }) => action === "preview"
    ? { ...preview, header, rows: [header.map(name => `Sample for ${name}`)] }
    : testSuccess);
  chooseFile(dialog, "wide.csv");
  await vi.waitFor(() => expect(mappings(dialog)).toHaveLength(12));
  for (const select of mappings(dialog).slice(2)) change(select, "");
  dialog.querySelector<HTMLButtonElement>(".impex-preview-navigation button:last-child")!.click();
  expect(mappings(dialog)).toHaveLength(2);
  expect(testButton(dialog).disabled).toBe(true);
  for (const select of mappings(dialog)) change(select, "");
  testButton(dialog).click();
  await vi.waitFor(() => expect(fileCalls[1]?.document["fields"]).toEqual(["name", "partner_id", ...Array(12).fill(null)]));
});

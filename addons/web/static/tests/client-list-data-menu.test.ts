/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { saveFile } from "../src/client/file-save.js";
import { csvDecode, csvEncode, decodeImpexRights, importColumns, listDataMenu, type ListDataMenuOptions } from "../src/client/list-data-menu.js";
import type { RpcClient } from "../src/client/transport.js";

vi.mock("../src/client/file-save.js", () => ({ saveFile: vi.fn() }));

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });

interface Call { model: string; method: string; args: Record<string, unknown>; options: unknown }
function fakeRpc(answer: (call: Call) => unknown): { rpc: RpcClient; calls: Call[] } {
  const calls: Call[] = [];
  const rpc = { call: vi.fn(async (model: string, method: string, args: Record<string, unknown>, options?: unknown) => { const call = { model, method, args, options }; calls.push(call); return answer(call); }) };
  return { rpc: rpc as unknown as RpcClient, calls };
}

const fields = [{ name: "name", label: "Name" }, { name: "partner_id", label: "Customer" }];

function menu(rpc: RpcClient, extra: Partial<ListDataMenuOptions> = {}): { node: HTMLElement; dispose(): void; options: ListDataMenuOptions } {
  const options: ListDataMenuOptions = {
    rpc, model: "test.note", context: { lang: "en_US" }, dialogs, canImport: true, fields,
    exportIds: vi.fn(async () => [5, 6]), imported: vi.fn(async () => {}), notice: vi.fn(), report: vi.fn(), ...extra,
  };
  const built = listDataMenu(options);
  document.body.append(built.node);
  return { ...built, options };
}

const open = (node: HTMLElement): void => node.querySelector<HTMLButtonElement>(".list-data-toggle-btn")!.click();
const entries = (node: HTMLElement): string[] => [...node.querySelectorAll(".list-data-menu button")].map(item => item.textContent ?? "");

test("CSV encoding quotes what needs quoting and guards formula-like text", () => {
  expect(csvEncode([["Name", "Note"], ["A, B", "say \"hi\""], ["=SUM(A1)", -5], [false, true], ["line\nbreak", null]]))
    .toBe("Name,Note\r\n\"A, B\",\"say \"\"hi\"\"\"\r\n'=SUM(A1),-5\r\n,true\r\n\"line\nbreak\",\r\n");
  expect(() => csvEncode([[{}]])).toThrow("neither text, a number nor a truth value");
});

test("CSV decoding reads quoted cells and line breaks, drops a byte order mark and blank lines, and refuses broken quotes", () => {
  expect(csvDecode("\ufeffName,Note\r\n\"A, B\",\"two\nlines \"\"q\"\"\"\n\nC,\r\n")).toEqual([["Name", "Note"], ["A, B", "two\nlines \"q\""], ["C", ""]]);
  expect(csvDecode("a,b")).toEqual([["a", "b"]]);
  expect(() => csvDecode("a,\"b\n")).toThrow("Line 1 of the file opens a quote that is never closed.");
  expect(() => csvDecode("a,\"b\"c\n")).toThrow("Line 1 of the file has text after a closing quote.");
  expect(() => csvDecode("a,b\"c\n")).toThrow("Line 1 of the file has a quote inside an unquoted value.");
});

test("import columns accept field names, paths and labels, and refuse unknown or repeated columns by name", () => {
  expect(importColumns([" Name ", "customer"], fields)).toEqual(["name", "partner_id"]);
  expect(importColumns(["name", "partner_id/id"], fields)).toEqual(["name", "partner_id/id"]);
  expect(() => importColumns(["Name", "Colour", ""], fields)).toThrow("These columns are not fields of this list: Colour, (empty).");
  expect(() => importColumns(["Name", "name"], fields)).toThrow("The column name appears twice.");
  expect(() => importColumns([""], fields)).toThrow("The first line of the file must name the columns.");
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

test("Export writes the chosen columns of the records as CSV with their labels", async () => {
  const save = vi.mocked(saveFile);
  const { rpc, calls } = fakeRpc(({ method }) => method === "erp_impex_rights" ? { export: true, import: false } : { datas: [["First", "Alice"], ["=2+2", false]] });
  const { node, options } = menu(rpc);
  open(node);
  await vi.waitFor(() => expect(entries(node)).toEqual(["Export"]));
  node.querySelector<HTMLButtonElement>(".list-export-btn")!.click();
  const dialog = document.querySelector<HTMLDialogElement>("dialog[open]")!;
  for (const box of dialog.querySelectorAll<HTMLInputElement>("input[type='checkbox']")) box.click();
  dialog.querySelector<HTMLButtonElement>("[data-export]")!.click();
  await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(1));
  expect(calls[1]).toEqual({ model: "test.note", method: "export_data", args: { ids: [5, 6], fields_to_export: ["name", "partner_id"], context: { lang: "en_US", import_compat: false } }, options: undefined });
  const [blob, name] = save.mock.calls[0]!;
  expect(name).toBe("test.note.csv");
  const bytes = new Uint8Array(await blob.arrayBuffer());
  expect([...bytes.slice(0, 3)]).toEqual([0xef, 0xbb, 0xbf]); // a byte order mark, so spreadsheets read UTF-8
  expect(new TextDecoder().decode(bytes)).toBe("Name,Customer\r\nFirst,Alice\r\n'=2+2,\r\n");
  expect(options.notice).toHaveBeenCalledWith("2 records exported.");
});

function chooseFile(dialog: HTMLDialogElement, text: string): void {
  const input = dialog.querySelector<HTMLInputElement>("input[type='file']")!;
  Object.defineProperty(input, "files", { configurable: true, value: [new File([text], "notes.csv", { type: "text/csv" })] });
}

async function startImport(answer: unknown): Promise<{ dialog: HTMLDialogElement; calls: Call[]; options: ListDataMenuOptions }> {
  const { rpc, calls } = fakeRpc(({ method }) => method === "erp_impex_rights" ? { export: false, import: true } : answer);
  const { node, options } = menu(rpc);
  open(node);
  await vi.waitFor(() => expect(entries(node)).toEqual(["Import records"]));
  node.querySelector<HTMLButtonElement>(".list-import-btn")!.click();
  return { dialog: document.querySelector<HTMLDialogElement>("dialog[open]")!, calls, options };
}

test("Import sends the file's rows to load with the mapped columns and reloads the list", async () => {
  const { dialog, calls, options } = await startImport({ ids: [7, 8], messages: [], nextrow: 0 });
  chooseFile(dialog, "Name,Customer\r\nFirst,Alice\r\nSecond,\r\n");
  dialog.querySelector<HTMLButtonElement>("[data-import]")!.click();
  await vi.waitFor(() => expect(options.imported).toHaveBeenCalledWith(2));
  expect(calls[1]).toEqual({ model: "test.note", method: "load", args: { fields: ["name", "partner_id"], data: [["First", "Alice"], ["Second", ""]], context: { lang: "en_US" } }, options: { write: true } });
  expect(options.notice).toHaveBeenCalledWith("2 records imported.");
  expect(dialog.open).toBe(false);
});

test("Import shows the server's messages with their file lines and imports nothing", async () => {
  const { dialog, options } = await startImport({ ids: false, messages: [{ type: "error", rows: { from: 1, to: 1 }, message: "Missing required value for the field 'Name'" }], nextrow: 0 });
  chooseFile(dialog, "Name\r\nFirst\r\n\"\"\r\n");
  dialog.querySelector<HTMLButtonElement>("[data-import]")!.click();
  await vi.waitFor(() => expect(dialog.querySelector(".import-dialog-status")!.textContent).toContain("Line 3: Missing required value for the field 'Name'"));
  expect(dialog.querySelector(".import-dialog-status")!.textContent).toContain("Nothing was imported.");
  expect(options.imported).not.toHaveBeenCalled();
  expect(dialog.open).toBe(true);
});

test("Import refuses a file with an unknown column or a short row before calling the server", async () => {
  const { dialog, calls, options } = await startImport({ ids: [], messages: [], nextrow: 0 });
  chooseFile(dialog, "Name,Colour\r\nFirst,Red\r\n");
  dialog.querySelector<HTMLButtonElement>("[data-import]")!.click();
  await vi.waitFor(() => expect(dialog.querySelector(".import-dialog-status")!.textContent).toBe("These columns are not fields of this list: Colour."));
  chooseFile(dialog, "Name,Customer\r\nFirst\r\n");
  dialog.querySelector<HTMLButtonElement>("[data-import]")!.click();
  await vi.waitFor(() => expect(dialog.querySelector(".import-dialog-status")!.textContent).toBe("Line 2 has 1 values; the first line names 2 columns."));
  expect(calls.map(call => call.method)).toEqual(["erp_impex_rights"]);
  expect(options.report).not.toHaveBeenCalled();
});

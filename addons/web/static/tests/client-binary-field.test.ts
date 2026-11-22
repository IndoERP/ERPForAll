/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, describe, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import { initializeAttachmentPolicy } from "../src/client/attachment-policy.js";
import { describeViews, type Field } from "../src/client/metadata.js";
import { resolveFieldWidget } from "../src/client/registry.js";
import { ClientError, type RpcClient } from "../src/client/transport.js";
import { RecordState } from "../src/client/record.js";

const PDF = "JVBERi0xLjQK"; // "%PDF-1.4\n"
const NAME = { model: "m", name: "document_name" };

function makeField(options: Record<string, unknown> = {}, extra: Partial<Field> = {}): Field {
  return { name: "document", label: "Document", kind: "binary", widget: "binary", options, readonly: false, required: false, relation: null, relationField: null, selection: null, ...extra };
}
interface MountOptions { values?: Record<string, unknown>; viewContext?: "form" | "list" | "card"; requestCap?: number; read?: () => Promise<unknown>; noChangeValues?: boolean }
function mount(field: Field, value: unknown, { values = {}, viewContext, requestCap, read, noChangeValues }: MountOptions = {}) {
  const change = vi.fn(), changeValues = vi.fn(), jobs: Promise<void>[] = [], errors: unknown[] = [];
  const call = vi.fn(read ?? (async () => []));
  const rpc = { call } as unknown as RpcClient;
  if (requestCap !== undefined) initializeAttachmentPolicy(rpc, null, requestCap);
  const run = (job: () => Promise<void>) => { jobs.push(job().catch(error => { errors.push(error); })); };
  const handle = createFieldControl({ field, value, values, model: "m", rpc, context: { lang: "en_US" }, change, ...(noChangeValues ? {} : { changeValues }), run, ...(viewContext ? { viewContext } : {}) });
  document.body.append(handle.node);
  return { handle, change, changeValues, call, errors, settle: () => Promise.all(jobs) };
}
function pick(node: HTMLElement, file: File): void {
  const input = node.querySelector<HTMLInputElement>("input[type=file]")!;
  Object.defineProperty(input, "files", { value: [file], configurable: true });
  input.dispatchEvent(new Event("change"));
}
const pdf = (name = "letter.pdf", bytes = 9) => new File([new Uint8Array(bytes).fill(37)], name, { type: "application/pdf" });
const fieldError = (node: HTMLElement) => node.querySelector(".field-error")!.textContent;
const download = (node: HTMLElement) => node.querySelector<HTMLAnchorElement>(".field-binary-download")!;
const action = (node: HTMLElement, name: string) => node.querySelector<HTMLButtonElement>(`.field-binary-${name}`);

/** Capture saveFile: the hidden link it presses and the blob behind it. */
function captureSaves(): { names: string[]; blobs: Blob[] } {
  const names: string[] = [], blobs: Blob[] = [];
  vi.stubGlobal("URL", Object.assign(Object.create(URL) as typeof URL, { createObjectURL: (blob: Blob) => { blobs.push(blob); return "blob:x"; }, revokeObjectURL: () => {} }));
  const press = HTMLAnchorElement.prototype.click;
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) { if (this.hidden) names.push(this.download); else press.call(this); });
  return { names, blobs };
}

afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("binary widget", () => {
  test("is registered for binary fields and is what a bare binary field renders", () => {
    expect(resolveFieldWidget("binary", "form").kinds).toEqual(["binary"]);
    const { handle } = mount(makeField({}, { widget: undefined }), false);
    expect(handle.node.querySelector(".field-binary")).not.toBeNull();
  });

  test("an empty field offers Upload only", () => {
    const { handle } = mount(makeField(), false);
    expect(handle.node.querySelector(".field-binary-name")!.textContent).toBe("No Document");
    expect(action(handle.node, "upload")!.getAttribute("aria-label")).toBe("Upload Document");
    expect(action(handle.node, "clear")!.hidden).toBe(true);
    expect(download(handle.node).hidden).toBe(true);
  });

  test("a stored file shows its name from the filename field, its size, Replace, Clear and Download", () => {
    const { handle } = mount(makeField({ filename: NAME }), "1.20 Kb", { values: { id: 7, document_name: "letter.pdf" } });
    expect(handle.node.querySelector(".field-binary-name")!.textContent).toBe("letter.pdf");
    expect(handle.node.querySelector(".field-binary-size")!.textContent).toBe("1.20 Kb");
    expect(action(handle.node, "upload")!.textContent).toBe("Replace");
    expect(action(handle.node, "clear")!.hidden).toBe(false);
    expect(download(handle.node).hidden).toBe(false);
  });

  test("upload stages the content and the file name in one change", async () => {
    const { handle, changeValues, change, settle, errors } = mount(makeField({ filename: NAME }), false, { values: { document_name: false } });
    pick(handle.node, pdf("scan.pdf"));
    await settle();
    expect(errors).toEqual([]);
    expect(changeValues).toHaveBeenCalledExactlyOnceWith({ document: "JSUlJSUlJSUl", document_name: "scan.pdf" });
    expect(change).not.toHaveBeenCalled();
  });

  test("upload without a filename option changes the field alone; replace does the same", async () => {
    const { handle, change, changeValues, settle } = mount(makeField(), PDF);
    expect(action(handle.node, "upload")!.textContent).toBe("Replace");
    pick(handle.node, pdf());
    await settle();
    expect(change).toHaveBeenCalledExactlyOnceWith("JSUlJSUlJSUl");
    expect(changeValues).not.toHaveBeenCalled();
  });

  test("clear empties the file and its name", () => {
    const { handle, changeValues } = mount(makeField({ filename: NAME }), PDF, { values: { document_name: "letter.pdf" } });
    action(handle.node, "clear")!.click();
    expect(changeValues).toHaveBeenCalledExactlyOnceWith({ document: false, document_name: false });
  });

  test("a file outside accepted_file_extensions, an empty file and a file over the request limit are refused on the field", async () => {
    const accepted = mount(makeField({ accepted_file_extensions: ".pdf" }), false);
    expect(accepted.handle.node.querySelector<HTMLInputElement>("input[type=file]")!.accept).toBe(".pdf");
    pick(accepted.handle.node, new File(["x"], "notes.txt", { type: "text/plain" }));
    expect(fieldError(accepted.handle.node)).toBe('"notes.txt" (text/plain) is not accepted here; accepted: .pdf.');
    pick(accepted.handle.node, pdf("empty.pdf", 0));
    expect(fieldError(accepted.handle.node)).toBe('"empty.pdf" is empty.');
    const limited = mount(makeField(), false, { requestCap: 65536 });
    pick(limited.handle.node, pdf("big.pdf", 70000));
    expect(fieldError(limited.handle.node)).toBe('"big.pdf" is 68.4 KiB; a file saved with the form can be up to 45 KiB.');
    await Promise.all([accepted.settle(), limited.settle()]);
    expect(accepted.change).not.toHaveBeenCalled(); expect(limited.change).not.toHaveBeenCalled();
  });

  test("a filename option without changeValues is refused when the field is editable", () => {
    expect(() => mount(makeField({ filename: NAME }), false, { noChangeValues: true })).toThrow(ClientError);
    expect(() => mount(makeField({ filename: NAME }, { readonly: true }), false, { noChangeValues: true })).not.toThrow();
  });

  test("readonly shows the file and Download, without Upload or Clear", () => {
    const { handle } = mount(makeField({}, { readonly: true }), PDF);
    expect(action(handle.node, "upload")).toBeNull();
    expect(action(handle.node, "clear")).toBeNull();
    expect(handle.node.querySelector("input[type=file]")).toBeNull();
    expect(download(handle.node).hidden).toBe(false);
  });

  test("download of content in the record saves it under the file name without a read", () => {
    const saves = captureSaves();
    const { handle, call } = mount(makeField({ filename: NAME }), PDF, { values: { document_name: "letter.pdf" } });
    download(handle.node).click();
    expect(saves.names).toEqual(["letter.pdf"]);
    expect(call).not.toHaveBeenCalled();
  });

  test("download of a size-only value reads the content once with the record's context", async () => {
    const saves = captureSaves();
    let answer!: (rows: unknown) => void;
    const { handle, call } = mount(makeField({ filename: NAME }), "9.00 bytes", { values: { id: 7, document_name: "letter.pdf" }, read: () => new Promise(resolve => { answer = resolve; }) });
    download(handle.node).click();
    download(handle.node).click();
    await vi.waitFor(() => expect(call).toHaveBeenCalled());
    expect(call).toHaveBeenCalledOnce();
    expect(call.mock.calls[0]).toEqual(["m", "read", { ids: [7], fields: ["document"], context: { lang: "en_US", bin_size: false } }, expect.objectContaining({ background: true })]);
    answer([{ id: 7, document: PDF }]);
    await vi.waitFor(() => expect(saves.names).toEqual(["letter.pdf"]));
  });

  test("a value that is neither content nor a size is refused", () => {
    expect(() => mount(makeField(), 42)).toThrow("Field document holds a value that is neither file content nor a size.");
  });

  test("in a list the cell is the file name as a download link, or nothing", () => {
    const named = mount(makeField({ filename: NAME }, { readonly: true }), "1.20 Kb", { values: { id: 7, document_name: "letter.pdf" }, viewContext: "list" });
    expect(download(named.handle.node).textContent).toBe("letter.pdf");
    expect(download(named.handle.node).hidden).toBe(false);
    expect(named.handle.node.querySelector("button")).toBeNull();
    const empty = mount(makeField({ filename: NAME }, { readonly: true }), false, { values: { id: 8, document_name: false }, viewContext: "list" });
    expect(download(empty.handle.node).hidden).toBe(true);
  });
});

describe("binary widget metadata and save", () => {
  const wire = (options: Record<string, unknown>, required = false) => ({ notices: [], wire_major: 1, registry_version: "r1", description_version: "d1", model: "m", fields: { m: {
    document: { type: "binary", label: "Document", readonly: false, required, selection: null, relation: null, relation_field: null },
    document_name: { type: "char", label: "File name", readonly: false, required: false, selection: null, relation: null, relation_field: null },
  } }, views: [{ identity: { kind: "stored", id: 1 }, type: "form", renderer: "web.form", schema_major: 1, root: 0, nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "field", field: { model: "m", name: "document" }, widget: "binary", label: "Document", force_save: false, modifiers: { readonly: null, required: null, invisible: null }, options },
  ] }] });

  test("the filename field is read with the file and stays writable", () => {
    const view = describeViews(wire({ filename: NAME }), "m", "r1").views.get("form")!;
    expect(view.fields.map(field => [field.name, field.readonly])).toEqual([["document", false], ["document_name", false]]);
  });

  test("a filename option naming a field outside the description is refused", () => {
    expect(() => describeViews(wire({ filename: { model: "m", name: "missing" } }), "m", "r1")).toThrow("Invalid field dependency: filename.");
  });

  test("an untouched size-only value is not written back; a new file and its name are", async () => {
    const view = describeViews(wire({ filename: NAME }), "m", "r1").views.get("form")!;
    const saved: unknown[] = [];
    const rpc = { call: vi.fn(async (_model: string, method: string, args: Record<string, unknown>) => {
      if (method === "web_save") saved.push(args["vals"]);
      return [{ id: 7, document: "9.00 bytes", document_name: "letter.pdf" }];
    }) } as unknown as RpcClient;
    const record = new RecordState(rpc, "m", view.fields, {});
    await record.load(7);
    record.change("document_name", "renamed.pdf");
    await record.save();
    record.changeMany({ document: PDF, document_name: "new.pdf" });
    await record.save();
    expect(saved).toEqual([{ document_name: "renamed.pdf" }, { document: PDF, document_name: "new.pdf" }]);
  });

  test("a required file blocks the save while it is empty", async () => {
    const view = describeViews(wire({}, true), "m", "r1").views.get("form")!;
    const rpc = { call: vi.fn(async () => [{ id: 7, document: false, document_name: false }]) } as unknown as RpcClient;
    const record = new RecordState(rpc, "m", view.fields, {});
    await record.load(7);
    await expect(record.save()).rejects.toThrow("Document is required.");
  });
});

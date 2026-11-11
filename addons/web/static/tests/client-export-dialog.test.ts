/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { exportDialog } from "../src/client/export-dialog.js";

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });

test("export refuses empty fields with danger, locks during download and restores on success", async () => {
  let finish!: () => void; const download = vi.fn(() => new Promise<void>(resolve => { finish = resolve; })); const notice = vi.fn();
  const handle = exportDialog({ dialogs, fields: [{ name: "name", label: "Name" }], formats: [{ key: "csv", label: "CSV" }], download, notice });
  const submit = handle.dialog.querySelector<HTMLButtonElement>("[data-export]")!;
  submit.click(); expect(notice).toHaveBeenCalledWith("Choose at least one field to export.", { level: "danger" }); expect(download).not.toHaveBeenCalled();
  handle.dialog.querySelector<HTMLInputElement>("input[type='checkbox']")!.click(); submit.click(); submit.click();
  expect(download).toHaveBeenCalledTimes(1); expect(download).toHaveBeenCalledWith(["name"], "csv");
  expect(submit.disabled).toBe(true); expect(handle.dialog.getAttribute("aria-busy")).toBe("true");
  finish(); await vi.waitFor(() => expect(submit.disabled).toBe(false));
  expect(handle.dialog.getAttribute("aria-busy")).toBe("false");
});

test("download rejection is reported visibly and controls recover without automatic retry", async () => {
  const report = vi.fn(); const download = vi.fn(async () => { throw new Error("Download refused"); });
  const handle = exportDialog({ dialogs, fields: [{ name: "name", label: "Name" }], formats: [{ key: "csv", label: "CSV" }], download, notice: () => {}, report });
  handle.dialog.querySelector<HTMLInputElement>("input")!.click(); handle.dialog.querySelector<HTMLButtonElement>("[data-export]")!.click();
  await vi.waitFor(() => expect(handle.dialog.querySelector("[role='alert']")?.textContent).toBe("Download refused"));
  expect(report).toHaveBeenCalledTimes(1); expect(download).toHaveBeenCalledTimes(1);
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "Export dialog options must be a plain object."],
  [{ fields: [null] }, "Export fields must contain at most 256 unique named fields."],
  [{ formats: [null] }, "Export formats must contain 1 to 16 unique formats."],
  [{ fields: [{ name: "a", label: "A", unknown: true }] }, "Unknown export field option: unknown."],
  [{ fields: [{ name: "a".repeat(257), label: "A" }] }, "Export fields must contain at most 256 unique named fields."],
  [{ fields: [{ name: "a", label: "A".repeat(8193) }] }, "Export fields must contain at most 256 unique named fields."],
  [{ notice: null }, "An export notice callback is required."],
  [{ report: true }, "Export report must be a function."],
  [{ signal: {} }, "Dialog signal must be an AbortSignal."],
  [{ unknown: true }, "Unknown export dialog option: unknown."],
])("invalid export declarations return typed admission errors: %j", (patch, message) => {
  const options = patch === null ? null : { dialogs, fields: [{ name: "name", label: "Name" }], formats: [{ key: "csv", label: "CSV" }], download: async () => {}, notice: () => {}, ...patch };
  expect(() => exportDialog(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
  expect(document.querySelector("dialog")).toBeNull();
});

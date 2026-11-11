/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { button } from "../src/client/dom.js";

let dialogs: DialogService;
const reported: unknown[] = [];
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });
  reported.length = 0; dialogs = new DialogService(error => reported.push(error));
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });

/// (partial: declared shell/header/footer/size; actual phone geometry is browser-owned)
test("plain shells use unique labels, declared size and body/footer slots", () => {
  const body = document.createElement("div"); body.textContent = "Body";
  const footer = document.createElement("div"); footer.append(button("Finish", () => {}));
  const first = dialogs.open({ title: "Details", body, footer, size: "lg" });
  expect(first.dialog.dataset["size"]).toBe("lg");
  expect(first.dialog.querySelector(".message-dialog-body")?.contains(body)).toBe(true);
  expect(first.dialog.querySelector(".message-dialog-footer")?.contains(footer)).toBe(true);
  const second = dialogs.open({ body: "Other" });
  expect(second.dialog.getAttribute("aria-labelledby")).not.toBe(first.dialog.getAttribute("aria-labelledby"));
  expect(first.dialog.querySelector("h2")?.textContent).toBe("Details");
  second.close();
  const minimal = dialogs.open({ body: "Body only", header: false });
  expect(minimal.dialog.querySelector("header")).toBeNull();
  expect(minimal.dialog.getAttribute("aria-label")).toBe("Details");
});

test("a scrollable dialog body is a keyboard tab stop", () => {
  const handle = dialogs.open({ title: "Long notice", body: "Long text ".repeat(100), size: "sm" });
  const body = handle.dialog.querySelector<HTMLElement>(".message-dialog-body")!;
  expect(body.tabIndex).toBe(0);
  body.focus(); expect(document.activeElement).toBe(body);
});

test("only the top dialog is active, closing reactivates its parent, final close clears modal state", () => {
  const launcher = button("Open", () => {}); document.body.append(launcher); launcher.focus();
  const first = dialogs.open({ title: "Parent", body: "Parent body" });
  const parentControl = first.dialog.querySelector<HTMLButtonElement>("button")!;
  const second = dialogs.open({ title: "Child", body: "Child body" });
  expect(first.dialog.inert).toBe(true);
  expect(first.dialog.getAttribute("aria-hidden")).toBe("true");
  expect(document.body.classList.contains("message-modal-open")).toBe(true);
  second.close();
  expect(first.dialog.inert).toBe(false);
  expect(first.dialog.hasAttribute("aria-hidden")).toBe(false);
  expect(document.activeElement).toBe(parentControl);
  expect(document.body.classList.contains("message-modal-open")).toBe(true);
  first.close();
  expect(document.body.classList.contains("message-modal-open")).toBe(false);
  expect(document.activeElement).toBe(launcher);
});

test("confirmation serializes callbacks, retains false, and dismisses through Escape policy", async () => {
  let finish!: (value: boolean) => void;
  const confirm = vi.fn(() => new Promise<boolean>(resolve => { finish = resolve; }));
  const cancel = vi.fn(); const dismiss = vi.fn();
  const handle = dialogs.confirm({ message: "Proceed?", confirm, cancel, dismiss });
  const controls = handle.dialog.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button");
  controls[0]!.click(); controls[0]!.click();
  expect(confirm).toHaveBeenCalledTimes(1);
  expect([...controls].every(control => control.disabled)).toBe(true);
  handle.dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
  expect(dismiss).not.toHaveBeenCalled();
  finish(false); await vi.waitFor(() => expect(controls[0]!.disabled).toBe(false));
  expect(handle.dialog.isConnected).toBe(true);
  handle.dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
  await expect(handle.result).resolves.toBe("dismiss");
  expect(dismiss).toHaveBeenCalledTimes(1); expect(cancel).not.toHaveBeenCalled();
  const fallback = dialogs.confirm({ message: "Again?", cancel });
  fallback.dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
  await fallback.result; expect(cancel).toHaveBeenCalledTimes(1);
});

test("confirmation rejection closes, rejects to the caller and reports once", async () => {
  const error = new Error("No access");
  const handle = dialogs.confirm({ message: "Proceed?", confirm: async () => { throw error; } });
  handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-footer button")!.click();
  await expect(handle.result).rejects.toBe(error);
  expect(handle.dialog.isConnected).toBe(false); expect(reported).toEqual([error]);
});

test("alert has a default title, small size, content class and conditional cancel", async () => {
  const alert = dialogs.alert({ message: "Read this", contentClass: "message-warning" });
  expect(alert.dialog.dataset["size"]).toBe("sm");
  expect(alert.dialog.querySelector("h2")?.textContent).toBe("Notice");
  expect(alert.dialog.querySelector(".message-dialog-body")?.classList.contains("message-warning")).toBe(true);
  expect(alert.dialog.querySelectorAll(".message-dialog-footer button")).toHaveLength(1);
  alert.dialog.querySelector<HTMLButtonElement>(".message-dialog-footer button")!.click();
  await expect(alert.result).resolves.toBe("confirm");
  const cancel = vi.fn(); const other = dialogs.alert({ title: "Attention", message: "More", cancel });
  expect(other.dialog.querySelectorAll(".message-dialog-footer button")).toHaveLength(2);
});

test("Ctrl+Enter uses an eligible first footer button and native disabled controls remain inert", () => {
  const run = vi.fn(); const footer = document.createElement("div"); const control = button("Run", run); footer.append(control);
  const handle = dialogs.open({ body: "Command", footer });
  handle.dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
  expect(run).toHaveBeenCalledTimes(1); control.disabled = true;
  handle.dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
  expect(run).toHaveBeenCalledTimes(1);
});

test("dialog limit refuses the ninth and parent disposal releases children", () => {
  const first = dialogs.open({ body: "First" });
  for (let i = 1; i < 8; i++) dialogs.open({ body: String(i) });
  expect(() => dialogs.open({ body: "Overflow" })).toThrowError(expect.objectContaining({ kind: "validation", message: "At most 8 dialogs may be open." }));
  first.dispose(); expect(document.querySelectorAll("dialog")).toHaveLength(0);
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "Dialog options must be a plain object."],
  [{ body: "Body", unexpected: true }, "Unknown dialog option: unexpected."],
  [{ body: "Body", header: "yes" }, "Dialog header must be a boolean."],
  [{ body: "Body", footer: "footer" }, "Dialog footer must be a trusted element."],
  [{ body: "Body", dismiss: true }, "Dialog dismiss must be a function."],
  [{ body: "Body", onClose: "close" }, "Dialog onClose must be a function."],
  [{ body: "Body", signal: {} }, "Dialog signal must be an AbortSignal."],
  [{ body: "Body", size: null }, "Unknown dialog size."],
])("open rejects invalid options before mounting: %j", (options, message) => {
  expect(() => dialogs.open(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
  expect(document.querySelector("dialog")).toBeNull();
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "Attached dialog options must be a plain object."],
  [{ unknown: true }, "Unknown attached dialog option: unknown."],
  [{ dismiss: false }, "Dialog dismiss must be a function."],
  [{ signal: {} }, "Dialog signal must be an AbortSignal."],
])("attach rejects invalid options before mounting: %j", (options, message) => {
  expect(() => dialogs.attach(document.createElement("dialog"), options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
  expect(document.querySelector("dialog")).toBeNull();
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "Confirmation options must be a plain object."],
  [{ message: "Body", unknown: true }, "Unknown confirmation option: unknown."],
  [{ message: "Body", confirm: true }, "Dialog confirm must be a function."],
  [{ message: "Body", cancel: true }, "Dialog cancel must be a function."],
  [{ message: "Body", contentClass: null }, "Invalid dialog content class."],
  [{ message: "Body", confirmLabel: 5 }, "Dialog confirmLabel must be text of at most 8192 bytes."],
])("confirm and alert reject invalid options: %j", (options, message) => {
  expect(() => dialogs.confirm(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
  expect(() => dialogs.alert(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
});

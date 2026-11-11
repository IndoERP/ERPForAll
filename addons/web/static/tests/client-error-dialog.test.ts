/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { ErrorDialog } from "../src/client/error-dialog.js";
import { ClientError } from "../src/client/transport.js";
let controller: ErrorDialog;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function(this: HTMLDialogElement) { this.open = false; } });
  controller = new ErrorDialog();
});
afterEach(() => { controller.dispose(); document.body.replaceChildren(); vi.restoreAllMocks(); });

test("backend details are text, collapsed, and copied exactly without request data", async () => {
  const writeText = vi.fn(async () => {});
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  controller.show(new ClientError("validation", "Invalid record", 400, { endpoint: "/erp/json/2/res.users/web_read", status: 400, name: "ValidationError", debug: "<img src=x> authorized trace" }));
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!;
  expect(dialog.querySelector("details")?.open).toBe(false);
  expect(dialog.querySelector("img")).toBeNull();
  const text = dialog.querySelector("pre")!.textContent;
  expect(text).toContain("<img src=x> authorized trace");
  [...dialog.querySelectorAll("button")].find(b => b.textContent === "Copy details")!.click();
  await vi.waitFor(() => expect(writeText).toHaveBeenCalledExactlyOnceWith(text));
  expect(dialog.textContent).toContain("Details copied.");
});

test("JS stacks stay concealed without a server debug grant and repeated failures do not stack dialogs", () => {
  const error = new Error("Renderer failed"); error.stack = "private stack";
  controller.show(error); controller.show(new Error("Second failure"));
  expect(document.querySelectorAll("dialog")).toHaveLength(1);
  expect(document.querySelector("pre")?.textContent).toBe("Error\nRenderer failed");
  expect(document.querySelector("[data-error-repeated]")?.textContent).toContain("Additional failures: 1");
  document.querySelector("dialog")!.dispatchEvent(new Event("cancel", { cancelable: true }));
  expect(document.querySelector("dialog")).toBeNull();
});

test("clipboard denial is visible and ambiguous writes offer no retry action", async () => {
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: vi.fn(async () => { throw new Error("denied"); }) } });
  controller.show(new ClientError("ambiguous", "Check the record before trying again."));
  const dialog = document.querySelector("dialog")!;
  expect(dialog.querySelector("h2")?.textContent).toBe("Check whether your change was saved");
  expect([...dialog.querySelectorAll("button")].map(b => b.textContent)).toEqual(["Copy details", "Close"]);
  dialog.querySelector<HTMLButtonElement>("details button")!.click();
  await vi.waitFor(() => expect(dialog.textContent).toContain("Copy failed. Select and copy the details above."));
});

/// (partial: the dialog shows the Error ID outside the collapsed details and copies it)
test("the Error ID is visible and part of the copied details", async () => {
  const writeText = vi.fn(async (_text: string) => {});
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  const id = "0b7c2a4e-5d1f-4c3a-9e8b-1a2b3c4d5e6f";
  controller.show(new ClientError("validation", "Invalid record", 422, { endpoint: "/erp/json/2/test.book/lend", status: 422, requestId: id }));
  const dialog = document.querySelector<HTMLDialogElement>("dialog")!;
  const reference = dialog.querySelector(".error-request-id")!;
  expect(reference.closest("details")).toBeNull();
  expect(reference.textContent).toBe(`Error ID: ${id}`);
  [...dialog.querySelectorAll("button")].find(b => b.textContent === "Copy details")!.click();
  await vi.waitFor(() => expect(writeText).toHaveBeenCalledOnce());
  expect(writeText.mock.calls[0]![0]).toContain(`Error ID: ${id}`);
});

/// (partial: no id, no Error ID line)
test("an error without a server id shows no Error ID line", () => {
  controller.show(new Error("Renderer failed"));
  expect(document.querySelector(".error-request-id")).toBeNull();
});

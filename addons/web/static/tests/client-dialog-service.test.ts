/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { choose } from "../src/client/dom.js";

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
});
afterEach(() => { vi.restoreAllMocks(); document.body.replaceChildren(); });

test("nested choices require distinct heading identities and an inactive parent", async () => {
  const first = choose("First", "Parent", ["Continue"]);
  const second = choose("Second", "Child", ["Continue"]);
  const dialogs = [...document.querySelectorAll("dialog")];
  expect(dialogs[0]!.getAttribute("aria-labelledby")).not.toBe(dialogs[1]!.getAttribute("aria-labelledby"));
  expect(dialogs[0]!.hasAttribute("inert")).toBe(true);
  dialogs[1]!.querySelector("button")!.click();
  dialogs[0]!.querySelector("button")!.click();
  await Promise.all([first, second]);
});

test('a choice initially focuses its primary action for native keyboard activation',async()=>{
  const pending=choose('Confirm','Continue with the operation?',['Continue','Cancel']);
  const primary=document.querySelector<HTMLButtonElement>('dialog .message-dialog-footer button.primary')!;
  expect(document.activeElement).toBe(primary);primary.click();await expect(pending).resolves.toBe('Continue');
});

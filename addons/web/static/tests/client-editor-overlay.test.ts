/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { openEditorOverlay } from "../src/client/editor-overlay.js";
import { PopoverService } from "../src/client/popover.js";
import { DialogService } from "../src/client/dialog-service.js";

const stops: (() => void)[] = [];
Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
afterEach(() => { for (const stop of stops.splice(0).reverse()) stop(); document.body.replaceChildren(); vi.restoreAllMocks(); });
function fixture() {
  const anchor = document.createElement("button"), content = document.createElement("div"), control = document.createElement("input"); anchor.textContent = "Choose"; content.append(control); document.body.append(anchor); anchor.focus();
  const report = vi.fn(), popovers = new PopoverService(document.body), dialogs = new DialogService(report); stops.push(() => popovers.dispose(), () => dialogs.dispose());
  return { anchor, content, control, environment: { messages: { popovers, dialogs }, report } };
}
test("desktop overlay uses shared popover and distinguishes selection from external cancellation", () => {
  const f = fixture(), onClose = vi.fn(); const first = openEditorOverlay({ anchor: f.anchor, content: f.content, environment: f.environment, title: "Choices", touch: false, onClose });
  expect(first.mode).toBe("popover"); expect(first.node.classList.contains("message-popover")).toBe(true);
  first.close("select"); expect(onClose).toHaveBeenLastCalledWith("select"); expect(document.activeElement).toBe(f.anchor);
  const second = openEditorOverlay({ anchor: f.anchor, content: f.content, environment: f.environment, title: "Choices", touch: false, onClose }); f.control.focus();
  document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true })); expect(second.node.isConnected).toBe(false); expect(onClose).toHaveBeenLastCalledWith("cancel");
});
test("touch overlay uses a native shared dialog sheet with disposal and focus return", () => {
  vi.spyOn(HTMLDialogElement.prototype, "showModal").mockImplementation(function(this: HTMLDialogElement) { this.open = true; });
  vi.spyOn(HTMLDialogElement.prototype, "close").mockImplementation(function(this: HTMLDialogElement) { this.open = false; });
  const f = fixture(), onClose = vi.fn(); const handle = openEditorOverlay({ anchor: f.anchor, content: f.content, environment: f.environment, title: "Choose a field", touch: true, onClose });
  expect(handle.mode).toBe("sheet"); expect(handle.node.tagName).toBe("DIALOG"); expect(handle.node.classList.contains("editor-sheet")).toBe(true); expect(handle.node.querySelector("h2")?.textContent).toBe("Choose a field");
  handle.dispose(); expect(handle.node.isConnected).toBe(false); expect(onClose).toHaveBeenCalledWith("dispose"); expect(document.activeElement).toBe(f.anchor);
});
test("owner abort emits dispose and never restores a detached trigger", () => {
  const f = fixture(), controller = new AbortController(), onClose = vi.fn();
  const handle = openEditorOverlay({ anchor: f.anchor, content: f.content, environment: { ...f.environment, signal: controller.signal }, title: "Choices", onClose });
  f.anchor.remove(); controller.abort(); expect(handle.node.isConnected).toBe(false); expect(onClose).toHaveBeenCalledOnce(); expect(onClose).toHaveBeenCalledWith("dispose");
});
test("overlay rejects invalid options with precise errors", () => {
  const f = fixture();
  expect(() => openEditorOverlay({ anchor: f.anchor, content: f.content, environment: f.environment, title: "Choices", touch: "yes" as unknown as boolean })).toThrowError(expect.objectContaining({ kind: "validation", message: "Editor overlay touch must be a boolean." }));
});

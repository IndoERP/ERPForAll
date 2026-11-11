/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { PopoverService } from "../src/client/popover.js";
import { DialogService } from "../src/client/dialog-service.js";

let service: PopoverService | undefined;
const showPopoverDescriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "showPopover"), hidePopoverDescriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "hidePopover");
const shown = new Set<HTMLElement>();
beforeEach(() => {
  // jsdom has no top layer. These platform stubs test service ownership/cleanup, not rendered geometry.
  Object.defineProperty(HTMLElement.prototype, "showPopover", { configurable: true, value(this: HTMLElement) { shown.add(this); this.style.display = "block"; } });
  Object.defineProperty(HTMLElement.prototype, "hidePopover", { configurable: true, value(this: HTMLElement) { shown.delete(this); this.style.display = "none"; } });
});
afterEach(() => {
  service?.dispose(); service = undefined; vi.restoreAllMocks(); document.body.replaceChildren(); shown.clear();
  if (showPopoverDescriptor) Object.defineProperty(HTMLElement.prototype, "showPopover", showPopoverDescriptor); else Reflect.deleteProperty(HTMLElement.prototype, "showPopover");
  if (hidePopoverDescriptor) Object.defineProperty(HTMLElement.prototype, "hidePopover", hidePopoverDescriptor); else Reflect.deleteProperty(HTMLElement.prototype, "hidePopover");
});
const fixture = () => {
  const anchor = document.createElement("button"); anchor.textContent = "Details";
  document.body.append(anchor); anchor.focus();
  vi.spyOn(anchor, "getBoundingClientRect").mockReturnValue({ left: 100, right: 180, top: 70, bottom: 100, width: 80, height: 30, x: 100, y: 70, toJSON() {} });
  service = new PopoverService(document.body);
  const content = document.createElement("button"); content.textContent = "Inspect";
  return { anchor, content, service };
};

test("anchored popover forwards placement/role, respects outside policy and focus restoration, and closes on navigation", () => {
  const { anchor, content, service } = fixture();
  const first = service.open(anchor, content, { placement: "bottom", role: "dialog", dismissOnOutside: false, restoreFocus: true });
  expect(first.node.getAttribute("role")).toBe("dialog");
  expect(first.node.dataset["placement"]).toBe("bottom");
  expect(first.node.style.position).toBe("fixed"); expect(first.node.style.top).toBe("108px");
  content.focus(); document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(first.node.isConnected).toBe(true);
  first.close(); expect(first.node.isConnected).toBe(false); expect(document.activeElement).toBe(anchor);
  const second = service.open(anchor, document.createElement("div"));
  document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(second.node.isConnected).toBe(false);
  const third = service.open(anchor, document.createElement("div"));
  service.navigation(); expect(third.node.isConnected).toBe(false);
});

test("anchor removal, abort and disposal release popovers without stale focus changes", async () => {
  const { anchor, service } = fixture(), abort = new AbortController();
  const popover = service.open(anchor, document.createElement("div"), { signal: abort.signal });
  abort.abort(); expect(popover.node.isConnected).toBe(false);
  const removed = service.open(anchor, document.createElement("div")); anchor.remove();
  await Promise.resolve(); expect(removed.node.isConnected).toBe(false);
  service.dispose(); service.dispose();
});

test("popover rejects detached anchors, wrong placement and excessive count with precise errors", () => {
  const { anchor, service } = fixture();
  expect(() => service.open(document.createElement("button"), document.createElement("div"))).toThrowError(expect.objectContaining({ kind: "validation", message: "Popover anchor must be connected inside its document." }));
  expect(() => service.open(anchor, document.createElement("div"), { placement: "wrong" as "top" })).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid popover placement." }));
  for (let i = 0; i < 16; i++) service.open(anchor, document.createElement("div"));
  expect(() => service.open(anchor, document.createElement("div"))).toThrowError(expect.objectContaining({ kind: "validation", message: "Popover limit of 16 reached." }));
});

test("popover fits viewport edges, repositions on resize, and respects disabled focus restoration", () => {
  const { anchor, content, service } = fixture();
  vi.spyOn(anchor, "getBoundingClientRect").mockReturnValue({ left: -100, right: -20, top: -50, bottom: -20, width: 80, height: 30, x: -100, y: -50, toJSON() {} });
  const view = service.open(anchor, content, { placement: "top", restoreFocus: false });
  expect(view.node.style.left).toBe("8px"); expect(view.node.style.top).toBe("8px");
  vi.mocked(anchor.getBoundingClientRect).mockReturnValue({ left: 140, right: 220, top: 170, bottom: 200, width: 80, height: 30, x: 140, y: 170, toJSON() {} });
  window.dispatchEvent(new Event("resize")); expect(view.node.style.left).toBe("180px");
  content.focus(); view.close(); expect(document.activeElement).not.toBe(anchor);
});

test("navigation closes every overlay even when a close callback fails", () => {
  const { anchor, service } = fixture();
  const first = service.open(anchor, document.createElement("div"));
  const failure = new Error("Close failed");
  const second = service.open(anchor, document.createElement("div"), { onClose: () => { throw failure; } });
  expect(() => service.navigation()).toThrowError(expect.objectContaining({ name: "AggregateError", message: "Popover close callbacks failed.", errors: [failure] }));
  expect(first.node.isConnected).toBe(false); expect(second.node.isConnected).toBe(false);
});


test.each([false, true])("MSG-R1: nested popover owns Escape before its %s guarded parent and restores focus", async guarded => {
  const show = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
  const close = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  const report = vi.fn(), dialogs = new DialogService(report);
  try {
    const launcher = document.createElement("button"); launcher.textContent = "Open parent"; document.body.append(launcher); launcher.focus();
    const anchor = document.createElement("button"); anchor.textContent = "Open details";
    const dismiss = vi.fn(() => guarded ? false : undefined);
    const parent = dialogs.open({ title: "Parent", body: anchor, dismiss }); anchor.focus();
    service = new PopoverService(document.body);
    const content = document.createElement("button"); content.textContent = "Details action";
    const popover = service.open(anchor, content, { restoreFocus: true }); content.focus();
    const first = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }); content.dispatchEvent(first);
    await Promise.resolve();
    expect(popover.node.isConnected).toBe(false); expect(parent.dialog.isConnected).toBe(true);
    expect(dismiss).not.toHaveBeenCalled(); expect(first.defaultPrevented).toBe(true); expect(document.activeElement).toBe(anchor);
    anchor.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    await vi.waitFor(() => expect(dismiss).toHaveBeenCalledTimes(1));
    expect(parent.dialog.isConnected).toBe(guarded);
    if (!guarded) expect(document.activeElement).toBe(launcher);
    expect(report).not.toHaveBeenCalled();
  } finally {
    service?.dispose(); dialogs.dispose();
    if (show) Object.defineProperty(HTMLDialogElement.prototype, "showModal", show); else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
    if (close) Object.defineProperty(HTMLDialogElement.prototype, "close", close); else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  }
});

test("MSG-R1: document capture closes only the newest popover across service owners", () => {
  const { anchor, service } = fixture();
  const first = service.open(anchor, document.createElement("button"));
  const other = new PopoverService(document.body);
  try {
    const content = document.createElement("button"); const second = other.open(anchor, content);
    content.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    expect(second.node.isConnected).toBe(false); expect(first.node.isConnected).toBe(true);
    first.node.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    expect(first.node.isConnected).toBe(false);
  } finally { other.dispose(); }
});

test("MSG-R1: a newer child modal outranks its parent's inactive popover", async () => {
  const show = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal"), close = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  const dialogs = new DialogService(() => {});
  try {
    const anchor = document.createElement("button"); anchor.textContent = "Anchor";
    const parent = dialogs.open({ title: "Parent", body: anchor }); anchor.focus(); service = new PopoverService(document.body);
    const content = document.createElement("button"); content.textContent = "Popover action";
    const popover = service.open(anchor, content, { restoreFocus: true }); content.focus();
    const child = dialogs.open({ title: "Child", body: "Child content" });
    child.dialog.querySelector("button")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    await vi.waitFor(() => expect(child.dialog.isConnected).toBe(false));
    expect(parent.dialog.isConnected).toBe(true); expect(popover.node.isConnected).toBe(true);
    content.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    expect(popover.node.isConnected).toBe(false); expect(parent.dialog.isConnected).toBe(true); expect(document.activeElement).toBe(anchor);
  } finally {
    service?.dispose(); dialogs.dispose();
    if (show) Object.defineProperty(HTMLDialogElement.prototype, "showModal", show); else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
    if (close) Object.defineProperty(HTMLDialogElement.prototype, "close", close); else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  }
});

test("editor popovers report distinct user and owner close reasons", async () => {
  const { anchor, service } = fixture(), reasons: string[] = [], onClose = (reason: string): void => { reasons.push(reason); };
  service.open(anchor, document.createElement("div"), { onClose }).close();
  const escape = service.open(anchor, document.createElement("button"), { onClose });
  escape.node.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  service.open(anchor, document.createElement("div"), { onClose }); document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  service.open(anchor, document.createElement("div"), { onClose }); service.navigation();
  const abort = new AbortController(); service.open(anchor, document.createElement("div"), { onClose, signal: abort.signal }); abort.abort();
  service.open(anchor, document.createElement("div"), { onClose }); anchor.remove(); await Promise.resolve();
  document.body.append(anchor); service.open(anchor, document.createElement("div"), { onClose }); service.dispose();
  expect(reasons).toEqual(["explicit", "escape", "outside", "navigation", "abort", "anchor-removed", "dispose"]);
});

test("editor sheet stays in the shared stack and fits visual viewport", () => {
  const { anchor, service } = fixture(); const sheet = service.open(anchor, document.createElement("button"), { presentation: "sheet" });
  expect(sheet.node.dataset["presentation"]).toBe("sheet"); expect(sheet.node.style.left).toBe("8px");
  expect(sheet.node.getAttribute("aria-modal")).not.toBe("true");
  sheet.node.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true })); expect(sheet.node.isConnected).toBe(false);
});

test("outside dismissal belongs only to the newest popover and rejects invalid presentation", () => {
  const { anchor, service } = fixture(); const firstClose = vi.fn(), secondClose = vi.fn();
  const first = service.open(anchor, document.createElement("div"), { onClose: firstClose });
  const second = service.open(anchor, document.createElement("div"), { onClose: secondClose }); document.body.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  expect(first.node.isConnected).toBe(true); expect(firstClose).not.toHaveBeenCalled(); expect(second.node.isConnected).toBe(false); expect(secondClose).toHaveBeenCalledWith("outside");
  expect(() => service.open(anchor, document.createElement("div"), { presentation: "unknown" as "sheet" })).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid popover presentation." }));
});

test("modal popover uses native manual top layer while retaining DOM and keyboard ownership", () => {
  const { anchor, service } = fixture(), modal = document.createElement("dialog"); modal.open = true; document.body.append(modal); modal.append(anchor);
  const content = document.createElement("button"), onClose = vi.fn(); const handle = service.open(anchor, content, { onClose });
  expect(handle.node.parentElement).toBe(modal); expect(handle.node.getAttribute("popover")).toBe("manual"); expect(shown.has(handle.node)).toBe(true);
  expect(handle.node.style.margin).toBe("0px"); expect(handle.node.style.right).toBe("auto");
  content.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  expect(handle.node.isConnected).toBe(false); expect(shown.has(handle.node)).toBe(false); expect(modal.isConnected).toBe(true); expect(onClose).toHaveBeenCalledWith("escape");
});

test("modal popover refuses missing native support explicitly without a clipped fallback", () => {
  const { anchor, service } = fixture(), modal = document.createElement("dialog"); modal.open = true; document.body.append(modal); modal.append(anchor);
  Reflect.deleteProperty(HTMLElement.prototype, "showPopover");
  expect(() => service.open(anchor, document.createElement("div"))).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Popovers inside dialogs require native popover support." }));
  expect(document.querySelector('.message-popover')).toBeNull();
});

test("modal native hide failure still removes listeners, DOM and owned callbacks", () => {
  const { anchor, service } = fixture(), modal = document.createElement("dialog"); modal.open = true; document.body.append(modal); modal.append(anchor);
  const failure = new Error("Native hide failed"), onClose = vi.fn(), handle = service.open(anchor, document.createElement("div"), { onClose });
  vi.spyOn(handle.node, "hidePopover").mockImplementation(() => { throw failure; });
  expect(() => handle.close()).toThrow(failure); expect(handle.node.isConnected).toBe(false); expect(onClose).toHaveBeenCalledTimes(1); handle.close();
  service.navigation(); expect(onClose).toHaveBeenCalledTimes(1);
});

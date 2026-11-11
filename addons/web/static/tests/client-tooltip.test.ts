/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { PopoverService } from "../src/client/popover.js";
import { TooltipService } from "../src/client/tooltip.js";
import { ClientError } from "../src/client/transport.js";
let tips: TooltipService | undefined, popovers: PopoverService | undefined;
afterEach(() => { tips?.dispose(); popovers?.dispose(); vi.useRealTimers(); document.body.replaceChildren(); });
function fixture(label = "Help") {
  vi.useFakeTimers(); const anchor = document.createElement("button"); anchor.textContent = label; document.body.append(anchor);
  popovers = new PopoverService(document.body); const report = vi.fn(); tips = new TooltipService(popovers, report); return { anchor, tips, report };
}
test("tooltip delay, leave, redundant text, trusted content, touch and removed target have scoped lifetimes", async () => {
  const { anchor } = fixture();
  const detach = tips!.attach(anchor, { text: "More information", delay: 200, touch: true });
  anchor.dispatchEvent(new MouseEvent("mouseenter")); vi.advanceTimersByTime(199); expect(document.querySelector('[role="tooltip"]')).toBeNull();
  vi.advanceTimersByTime(1); expect(document.querySelector('[role="tooltip"]')?.textContent).toBe("More information");
  expect(anchor.getAttribute("aria-describedby")).toBe(document.querySelector('[role="tooltip"]')?.id);
  anchor.dispatchEvent(new MouseEvent("mouseleave")); expect(document.querySelector('[role="tooltip"]')).toBeNull(); expect(anchor.hasAttribute("aria-describedby")).toBe(false);
  anchor.dispatchEvent(new Event("touchstart")); vi.advanceTimersByTime(200); expect(document.querySelector('[role="tooltip"]')).not.toBeNull();
  anchor.dispatchEvent(new Event("touchend")); expect(document.querySelector('[role="tooltip"]')).toBeNull();
  detach(); tips!.attach(anchor, { text: "Help", delay: 0 }); anchor.dispatchEvent(new MouseEvent("mouseenter")); vi.runOnlyPendingTimers(); expect(document.querySelector('[role="tooltip"]')).toBeNull();
  tips!.dispose(); popovers!.dispose();
  const second = fixture().anchor;
  tips!.attach(second, { content: () => { const span = document.createElement("span"); span.textContent = "Compiled help"; return span; }, delay: 0 });
  second.dispatchEvent(new MouseEvent("mouseenter")); vi.runOnlyPendingTimers(); expect(document.querySelector('[role="tooltip"]')?.textContent).toBe("Compiled help");
  second.remove(); await Promise.resolve(); expect(document.querySelector('[role="tooltip"]')).toBeNull();
});
test("tooltip disposal prevents late timers; malformed delays and empty declarations fail precisely", () => {
  const { anchor } = fixture();
  expect(() => tips!.attach(anchor, { text: "x", delay: NaN })).toThrowError(expect.objectContaining({ kind: "validation", message: "Tooltip delay must be finite and between 0 and 60000 milliseconds." }));
  expect(() => tips!.attach(anchor, {})).toThrowError(expect.objectContaining({ kind: "validation", message: "Tooltip requires text or trusted content." }));
  tips!.attach(anchor, { text: "Help text" }); anchor.dispatchEvent(new MouseEvent("mouseenter")); tips!.dispose(); vi.runAllTimers();
  expect(document.querySelector('[role="tooltip"]')).toBeNull(); expect(vi.getTimerCount()).toBe(0);
});


test.each(["factory", "invalid-element", "admission"] as const)("MSG-R4: async %s failure reports once after tooltip cleanup", failureKind => {
  const { anchor, report } = fixture(); anchor.setAttribute("aria-describedby", "existing-help");
  const failure = failureKind === "factory" ? new Error("Tooltip factory failed")
    : failureKind === "invalid-element" ? new ClientError("validation", "Popover content must be an element in the anchor document.")
    : new ClientError("validation", "Popover limit of 16 reached.");
  if (failureKind === "admission") for (let i = 0; i < 16; i++) popovers!.open(anchor, document.createElement("div"));
  tips!.attach(anchor, { delay: 0, content: () => {
    if (failureKind === "factory") throw failure;
    if (failureKind === "invalid-element") return null as unknown as HTMLElement;
    return document.createElement("span");
  } });
  anchor.dispatchEvent(new MouseEvent("mouseenter"));
  expect(() => vi.runOnlyPendingTimers()).not.toThrow();
  expect(report).toHaveBeenCalledTimes(1);
  if (failureKind === "factory") expect(report).toHaveBeenCalledWith(failure);
  else expect(report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: failure.message }));
  expect(document.querySelector('[role="tooltip"]')).toBeNull(); expect(anchor.getAttribute("aria-describedby")).toBe("existing-help");
  vi.runOnlyPendingTimers(); expect(report).toHaveBeenCalledTimes(1);
});

test.each([false, true])("MSG-R4: factory disposal suppresses late overlay creation and error presentation (throws=%s)", throws => {
  const { anchor, report } = fixture(), factory = vi.fn(() => { tips!.dispose(); if (throws) throw new Error("Owner already disposed"); return document.createElement("span"); });
  tips!.attach(anchor, { content: factory, delay: 0 }); anchor.dispatchEvent(new MouseEvent("mouseenter"));
  expect(() => vi.runOnlyPendingTimers()).not.toThrow();
  expect(factory).toHaveBeenCalledTimes(1); expect(report).not.toHaveBeenCalled();
  expect(document.querySelector('[role="tooltip"]')).toBeNull(); expect(anchor.hasAttribute("aria-describedby")).toBe(false);
  expect(vi.getTimerCount()).toBe(0);
});

test("MSG-R4: tooltip ownership requires an explicit reporter", () => {
  fixture();
  expect(() => new TooltipService(popovers!, undefined as unknown as (error: unknown) => void)).toThrowError(expect.objectContaining({ kind: "validation", message: "Tooltip service requires an owner error reporter." }));
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { NotificationService } from "../src/client/notifications.js";

const services: NotificationService[] = [];
const setup = () => { const report = vi.fn(); const service = new NotificationService(document.body, report); services.push(service); return { service, report }; };
afterEach(() => { services.splice(0).forEach(service => service.dispose()); vi.useRealTimers(); document.body.replaceChildren(); });

test.each(["info", "success", "warning", "danger"] as const)("notification renders %s severity and safe title/body", level => {
  const { service } = setup(); service.add("<b>Body</b>", { title: "Title", level, sticky: true });
  const notice = document.querySelector<HTMLElement>(".client-notification")!;
  expect(notice.dataset["level"]).toBe(level); expect(notice.textContent).toContain("Title"); expect(notice.textContent).toContain("<b>Body</b>"); expect(notice.querySelector("b")).toBeNull();
});
test("timed notice pauses on hover and resets duration on leave; sticky and nonpositive delay remain", () => {
  vi.useFakeTimers(); const { service } = setup();
  service.add("Timed", { delay: 100 }); service.add("Sticky", { sticky: true, delay: 10 }); service.add("No timer", { delay: 0 });
  const timed = document.querySelector<HTMLElement>(".client-notification")!;
  vi.advanceTimersByTime(90); timed.dispatchEvent(new MouseEvent("mouseenter")); vi.advanceTimersByTime(200); expect(timed.isConnected).toBe(true);
  timed.dispatchEvent(new MouseEvent("mouseleave")); vi.advanceTimersByTime(99); expect(timed.isConnected).toBe(true); vi.advanceTimersByTime(1); expect(timed.isConnected).toBe(false);
  expect(document.querySelectorAll(".client-notification")).toHaveLength(2);
});
test("native action and close controls have independent behavior and callback errors are reported", async () => {
  const { service, report } = setup(); const run = vi.fn(); const closed = vi.fn();
  const close = service.add("Choose", { sticky: true, actions: [{ label: "Review", run }], onClose: closed });
  const buttons = [...document.querySelectorAll<HTMLButtonElement>(".client-notification button")];
  expect(buttons).toHaveLength(2); expect(buttons.every(button => button.type === "button")).toBe(true);
  buttons.find(button => button.textContent === "Review")!.click(); await Promise.resolve(); expect(run).toHaveBeenCalledTimes(1); expect(document.querySelector(".client-notification")).not.toBeNull();
  buttons.find(button => button.getAttribute("aria-label") === "Close notification")!.click(); close(); expect(closed).toHaveBeenCalledTimes(1);
  const failure = new Error("action failed"); service.add("Failure", { sticky: true, actions: [{ label: "Fail", run: async () => { throw failure; } }] });
  [...document.querySelectorAll<HTMLButtonElement>("button")].find(button => button.textContent === "Fail")!.click(); await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failure));
});
test("different messages coexist; equal message replaces predecessor and closing one preserves others", () => {
  const { service } = setup(); const oldClose = vi.fn(); const old = service.add("Same", { sticky: true, onClose: oldClose });
  const other = service.add("Other", { sticky: true }); service.add("Same", { title: "New", sticky: true });
  expect(document.querySelectorAll(".client-notification")).toHaveLength(2); expect(oldClose).toHaveBeenCalledTimes(1); old(); expect(oldClose).toHaveBeenCalledTimes(1);
  other(); expect(document.querySelectorAll(".client-notification")).toHaveLength(1); expect(document.body.textContent).toContain("New");
});
test("notice is an assertive atomic alert with labelled controls", () => {
  const { service } = setup(); service.add("Announce", { actions: [{ label: "Open", run: () => {} }] });
  const notice = document.querySelector<HTMLElement>(".client-notification")!;
  expect(notice.getAttribute("role")).toBe("alert"); expect(notice.getAttribute("aria-live")).toBe("assertive"); expect(notice.getAttribute("aria-atomic")).toBe("true");
  expect(notice.querySelector('button[aria-label="Close notification"]')).not.toBeNull(); expect(notice.querySelector("button")?.textContent).toBe("Open");
});
test("notification count and UTF8 content limits refuse visibly and replacements release accounting", () => {
  const { service } = setup(); for (let i = 0; i < 32; i++) service.add(String(i), { sticky: true });
  expect(() => service.add("overflow")).toThrowError(expect.objectContaining({ kind: "validation", message: "Notification limit exceeded: 32 notices or 65536 UTF-8 bytes." }));
  expect(document.querySelector(".client-notification-overflow")?.textContent).toContain("More notifications");
  service.add("0", { title: "Replacement", sticky: true }); expect(document.querySelectorAll(".client-notification")).toHaveLength(32);
  const second = new NotificationService(document.body, () => {}); services.push(second);
  const close = second.add("é".repeat(32768), { sticky: true });
  expect(() => second.add("x")).toThrowError(expect.objectContaining({ kind: "validation", message: "Notification limit exceeded: 32 notices or 65536 UTF-8 bytes." }));
  close(); expect(() => second.add("x")).not.toThrow();
});
test("invalid notification options fail before mutation; disposal cancels timers and late callbacks", async () => {
  vi.useFakeTimers(); const { service, report } = setup();
  expect(() => service.add("x", { delay: NaN })).toThrowError(expect.objectContaining({ kind: "validation", message: "Notification delay must be a finite number from 0 to 2147483647." }));
  expect(() => service.add("x", { level: "bogus" as "info" })).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid notification severity." }));
  let reject!: (error: Error) => void; const pending = new Promise<void>((_, rejectPromise) => { reject = rejectPromise; });
  service.add("pending", { actions: [{ label: "Wait", run: () => pending }] }); document.querySelector<HTMLButtonElement>("button")!.click(); service.dispose(); reject(new Error("late")); await Promise.resolve(); await Promise.resolve();
  expect(report).not.toHaveBeenCalled(); expect(vi.getTimerCount()).toBe(0); expect(document.querySelector(".client-notifications")).toBeNull();
  expect(() => service.add("after")).toThrowError(expect.objectContaining({ kind: "validation", message: "Notification service is disposed." }));
});
test("close callback failures reach the owner once after removal and action callbacks serialize", async () => {
  const { service, report } = setup(); const failure = new Error("close failed");
  const close = service.add("Close failure", { sticky: true, onClose: () => { throw failure; } }); close(); close();
  expect(report).toHaveBeenCalledExactlyOnceWith(failure); expect(document.querySelector(".client-notification")).toBeNull();
  let finish!: () => void; const pending = new Promise<void>(resolve => { finish = resolve; }); const action = vi.fn(() => pending);
  service.add("Pending action", { sticky: true, actions: [{ label: "Continue", run: action }] }); const control = document.querySelector<HTMLButtonElement>(".client-notification-actions button")!;
  control.click(); control.click(); expect(action).toHaveBeenCalledTimes(1); expect(control.disabled).toBe(true); finish(); await vi.waitFor(() => expect(control.disabled).toBe(false));
});
test("reentrant close callbacks cannot leave untracked equal-message DOM or consume extra byte budget", () => {
  const { service } = setup();
  service.add("Same", { sticky: true, onClose: () => {
    service.add("Same", { title: "Nested", sticky: true, onClose: () => { service.add("Same", { title: "Nested again", sticky: true }); } });
  } });
  service.add("Same", { title: "Outer", sticky: true });
  expect(document.querySelectorAll(".client-notification")).toHaveLength(1);
  service.dispose(); expect(document.querySelector(".client-notification")).toBeNull();
});

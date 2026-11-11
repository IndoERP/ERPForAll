/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { TestScope } from "@erpforall/elang-test";
import { Ribbon, NotificationPermissionAlert, messageWidgetTemplates, noContentHelp } from "../src/client/message-widgets.js";
let scope: TestScope | undefined;
afterEach(async () => { await scope?.dispose(); scope = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); });
/// (partial: helper rendering; loaded versus loading/error integration belongs to the app test)
test("no-content help supplies alert semantics, separate title/body and generic safe guidance", () => {
  const specific = noContentHelp("No entries", "<script>Not markup</script>");
  expect(specific.getAttribute("role")).toBe("alert"); expect(specific.querySelector("h2")?.textContent).toBe("No entries");
  expect(specific.querySelector("p")?.textContent).toBe("<script>Not markup</script>"); expect(specific.querySelector("script")).toBeNull();
  const generic = noContentHelp(); expect(generic.textContent).toContain("No records yet"); expect(generic.textContent).toContain("Create a record");
});
/// (partial: client component; real-view declaration awaits)
test("ribbon component separates text and tooltip, applies semantic style and compacts long labels without an action role", async () => {
  scope = new TestScope();
  const { root } = await scope.mount(Ribbon, { templates: messageWidgetTemplates, props: { text: "A rather long business label", tooltip: "Status detail", backgroundClass: "bg-warning" } });
  const ribbon = root.querySelector<HTMLElement>(".message-ribbon")!;
  expect(ribbon.textContent).toBe("A rather long business label"); expect(ribbon.title).toBe("Status detail"); expect(ribbon.dataset["level"]).toBe("warning");
  expect(ribbon.classList.contains("message-ribbon-compact")).toBe(true); expect(root.querySelector("button")).toBeNull(); expect(ribbon.hasAttribute("role")).toBe(false);
});
test.each(["denied", "granted", "default", "unsupported"] as const)("permission component renders only denied state (%s) and opens guidance without granting permission", async permission => {
  const requestPermission = vi.fn(); vi.stubGlobal("Notification", { requestPermission }); const open = vi.fn(); scope = new TestScope();
  const { root } = await scope.mount(NotificationPermissionAlert, { templates: messageWidgetTemplates, props: { permission, open } });
  const control = root.querySelector<HTMLButtonElement>("button");
  expect(Boolean(control)).toBe(permission === "denied"); control?.click(); expect(open).toHaveBeenCalledTimes(permission === "denied" ? 1 : 0); expect(requestPermission).not.toHaveBeenCalled();
});
test("ribbon refuses an unknown background class with kind and message", async () => {
  scope = new TestScope(); await expect(scope.mount(Ribbon, { templates: messageWidgetTemplates, props: { text: "Status", backgroundClass: "untrusted-class" } })).rejects.toMatchObject({ kind: "validation", message: "Unsupported ribbon background class: untrusted-class." });
});

test.each(["info", "success", "warning", "danger"] as const)("ribbon component accepts %s semantic presentation without interpreting its text", async level => {
  scope = new TestScope();
  const { root } = await scope.mount(Ribbon, { templates: messageWidgetTemplates, props: { text: "<b>Status</b>", background: level } });
  expect(root.querySelector(".message-ribbon")?.getAttribute("data-level")).toBe(level);
  expect(root.textContent).toBe("<b>Status</b>"); expect(root.querySelector("b")).toBeNull();
});

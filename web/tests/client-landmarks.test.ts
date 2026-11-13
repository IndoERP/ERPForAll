/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/// <reference path="../../addons/web/static/src/client/templates.d.ts" />
import { afterEach, expect, test, vi } from "vitest";
import { unmount } from "../src/index.js";
import { boot } from "../../addons/web/static/src/client/index.js";
import { HumanApp } from "../../addons/web/static/src/client/app.js";
import { configureClientTemplates } from "../../addons/web/static/src/client/registry.js";
import { templates } from "./fixtures/client-landmark-templates.js";
import { bootstrap, description } from "./fixtures/client-landmark-data.js";
import { auditComponent } from "../testing/axe.js";

let app: HumanApp | undefined;
let mountedHost: HTMLElement | undefined;
afterEach(() => {
  app?.dispose(); app = undefined;
  if (mountedHost) { unmount(mountedHost); mountedHost = undefined; }
  document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks();
  history.replaceState(null, "", "/erp");
});
function page(data: "erp-login" | "erp-bootstrap", value: unknown): HTMLElement {
  const host = document.createElement("main"); host.id = "erp-root"; host.dataset["retained"] = "yes";
  const script = document.createElement("script"); script.type = "application/json"; script.id = data; script.textContent = JSON.stringify(value);
  document.body.append(host, script); return host;
}
function shell(): HTMLElement {
  const root = document.createElement("div"); root.append(templates.find(t => t.name === "web.HumanClient")!.render({})!);
  document.body.append(root); configureClientTemplates(templates); return root;
}
function mockRpc(views: unknown = description): void {
  vi.stubGlobal("fetch", vi.fn(async (raw: string, init?: RequestInit) => {
    const path = String(raw), body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
    const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
    const result = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : path.endsWith("erp_load_web_action_views") ? { action, views }
      : path.endsWith("erp_load_web_action") ? action : path.endsWith("get_views") ? views
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Café" }] }
      : path.endsWith("onchange") ? { value: body["values"] }
      : path.endsWith("default_get") ? { name: "" }
      : [{ id: 5, name: "Café" }];
    return new Response(JSON.stringify(result), { headers: { "Content-Type": "application/json" } });
  }));
}
function headings(root: ParentNode = document): string[] { return [...root.querySelectorAll("h1")].map(h => h.textContent ?? ""); }
const targetRules = new Set(["landmark-main-is-top-level", "landmark-no-duplicate-main", "landmark-unique", "empty-table-header", "page-has-heading-one", "heading-order"]);

test("boot normalizes the server mount before login; one top-level main and one h1 remain", async () => {
  page("erp-login", { csrf: "fixture", database: "fixture", redirect: "/erp", error: null });
  await boot();
  const host = document.getElementById("erp-root")!; mountedHost = host;
  expect(host.tagName).toBe("DIV"); expect(host.dataset["retained"]).toBe("yes");
  expect(document.querySelectorAll("main")).toHaveLength(1);
  expect(document.querySelector("main")!.parentElement!.closest("main,nav,aside,section[aria-label]")).toBeNull();
  expect(headings()).toEqual(["Sign in"]);
  const report = await auditComponent(document.body);
  expect(report.violations.filter(v => targetRules.has(v.id))).toEqual([]);
});
test("boot keeps a neutral mount and renders a semantic error page for invalid data", async () => {
  const root = page("erp-login", { csrf: "fixture", database: "fixture", redirect: "//invalid", error: null });
  const neutral = document.createElement("div"); neutral.id = root.id; root.replaceWith(neutral);
  await boot();
  expect(document.getElementById("erp-root")).toBe(neutral);
  expect(document.querySelectorAll("main")).toHaveLength(1); expect(headings()).toHaveLength(1);
  expect(document.querySelector('[role="alert"]')?.textContent).toBe("Invalid sign-in redirect.");
});
test("missing mount still rejects explicitly", async () => { await expect(boot()).rejects.toMatchObject({ name: "Error", message: "Application mount point is missing." }); });
test("home h1 belongs to the page and skip link still focuses content", async () => {
  history.replaceState(null, "", "/erp"); mockRpc(); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  expect(headings(root)).toEqual(["Apps"]);
  root.querySelector<HTMLButtonElement>("[data-skip-content]")!.click(); expect(document.activeElement).toBe(root.querySelector("main"));
  root.querySelector<HTMLButtonElement>("[data-app-switcher]")!.click(); expect(headings(root)).toEqual(["Apps"]);
});
test("list header is named, selection works, and opening the launcher adds no page h1", async () => {
  history.replaceState(null, "", "/erp/action-20"); mockRpc(); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  expect(headings(root)).toEqual(["Notes"]);
  const th = root.querySelector<HTMLTableCellElement>(".list-record-selector")!;
  expect(th.textContent?.trim()).toBe("Select records"); expect(th.scope).toBe("col");
  const select = th.querySelector<HTMLInputElement>("input")!; select.click(); expect(root.querySelectorAll("tr.row-selected")).toHaveLength(1);
  root.querySelector<HTMLButtonElement>("[data-app-switcher]")!.click(); expect(headings(root)).toEqual(["Notes"]);
  root.querySelector<HTMLButtonElement>('[aria-label="ERPForAll home"]')!.click();
  await vi.waitFor(() => expect(headings(root)).toEqual(["Apps"]));
});
test("legacy record forms retain one named page heading through onchange", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form"); mockRpc(); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  expect(headings(root)).toEqual(["Café"]);
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!; input.value = "Changed"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  // Preserve the existing breadcrumb policy: keep the saved record label during unsaved editing.
  expect(headings(root)).toEqual(["Café"]);
});
test("New forms have one meaningful page heading without a record label", async () => {
  history.replaceState(null, "", "/erp/action-20?view_type=form"); mockRpc(); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  expect(headings(root)).toEqual(["New"]);
});
test("declared title stays in its sheet with no duplicate page h1", async () => {
  const views = { ...description, views: [{ ...description.views[0]!, nodes: [
    { kind: "form", record_mode: "record", children: [1] },
    { kind: "sheet", children: [2] }, { kind: "title", children: [3] },
    { kind: "heading", level: 1, children: [4] }, description.views[0]!.nodes[1]!
  ] }, ...description.views.slice(1)] };
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form"); mockRpc(views); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  expect(root.querySelectorAll("h1")).toHaveLength(1); expect(root.querySelector(".record-form .form-title-block h1 input")?.getAttribute("name")).toBe("name");
});
test("notebook title visibility transfers the page heading without moving fields", async () => {
  const views = { ...description, views: [{ ...description.views[0]!, nodes: [
    { kind: "form", record_mode: "record", children: [1] }, { kind: "sheet", children: [2] },
    { kind: "notebook", children: [3, 4] },
    { kind: "page", name: "overview", label: "Overview", children: [5] },
    { kind: "page", name: "title", label: "Title", children: [6] },
    description.views[0]!.nodes[1]!, { kind: "title", children: [7] },
    { kind: "heading", level: 1, children: [8] }, description.views[0]!.nodes[1]!
  ] }, ...description.views.slice(1)] };
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form"); mockRpc(views); const root = shell(); app = new HumanApp(root, bootstrap); await app.start();
  const visibleHeadings = () => [...root.querySelectorAll("h1")].filter(heading => !heading.closest("[hidden]"));
  expect(visibleHeadings()).toHaveLength(1); expect(visibleHeadings()[0]!.textContent).toBe("Café");
  const input = root.querySelector<HTMLInputElement>('.form-title-block input[name="name"]')!;
  const tabs = [...root.querySelectorAll<HTMLButtonElement>('[role="tab"]')]; tabs[1]!.click();
  await vi.waitFor(() => expect(visibleHeadings()).toHaveLength(1));
  expect(visibleHeadings()[0]!.contains(input)).toBe(true); expect(input.closest('[role="tabpanel"]')?.hasAttribute("hidden")).toBe(false);
  tabs[0]!.click(); await vi.waitFor(() => expect(visibleHeadings()[0]!.textContent).toBe("Café"));
  root.querySelector<HTMLButtonElement>('[aria-label="ERPForAll home"]')!.click();
  await vi.waitFor(() => expect(headings(root)).toEqual(["Apps"]));
});
test("expired session replaces protected content with a main, h1 and sign-in link", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response('{"message":"Expired"}', { status: 401 })));
  const root = shell(); app = new HumanApp(root, bootstrap); await expect(app.start()).rejects.toMatchObject({ kind: "session", message: "Your session has expired. Sign in again." });
  expect(root.querySelectorAll("main")).toHaveLength(1); expect(headings(root)).toHaveLength(1);
  expect(root.textContent).not.toContain("Operator"); const signIn = new URL(root.querySelector("a")!.href);
  expect(signIn.pathname).toBe("/erp/login"); expect(signIn.searchParams.get("redirect")).toBe(window.location.pathname + window.location.search);
});

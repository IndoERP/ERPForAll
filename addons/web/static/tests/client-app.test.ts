/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { registerFieldWidget, registerDebugItemProvider, configureClientTemplates, registerCommandCategory, commandCategories, registerCommandProvider, commandProviders } from "../src/client/registry.js";
import { RecordForm } from "../src/client/record-form.js";
import { HumanApp } from "../src/client/app.js";
import { describeViews } from "../src/client/metadata.js";
import { ClientError } from "../src/client/transport.js";
import * as panelRecovery from "../src/client/panel-recovery.js";
import { getActiveCommandPalette } from "../src/client/command-palette.js";

/** Match the bundled navigation RPC while keeping standalone discovery fixtures. */
function actionMetadata(path: string, action: unknown, views: unknown): unknown {
  return path.endsWith("erp_load_web_action_views") ? { action, views } : action;
}

const bootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company" }],
  current_company_id: 1,
  allowed_company_ids: [1],
  active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};

const description = {
  wire_major: 1,
  registry_version: "r1",
  description_version: "d1",
  model: "test.note",
  fields: {
    "test.note": {
      name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null }
    }
  },
  notices: [],
  views: [
    {
      identity: { kind: "stored", id: 1 },
      type: "form",
      schema_major: 1,
      renderer: "web.form",
      root: 0,
      nodes: [
        { kind: "form", record_mode: "record", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    },
    {
      identity: { kind: "stored", id: 2 },
      type: "list",
      schema_major: 1,
      renderer: "web.list",
      root: 0,
      nodes: [
        { kind: "list", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    }
  ]
};

let app: HumanApp | undefined;
const showModalDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const closeDialogDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
});

// Controller fixture only; the asset verification producer separately compiles production XML.
beforeEach(() => configureClientTemplates([{
  name: "web.HumanControlPanel",
  render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
      const node = document.createElement("div");
      node.setAttribute(`data-control-${key}`, "");
      root.append(node);
    }
    return root;
  }
}]));

afterEach(() => {
  app?.dispose();
  app = undefined;
  if (showModalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", showModalDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (closeDialogDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDialogDescriptor);
  else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
  vi.restoreAllMocks();
  document.body.replaceChildren();
  document.body.style.overflow = "";
  delete document.documentElement.dataset["officeTheme"];
  vi.unstubAllGlobals();
  history.replaceState(null, "", "/");
});

function host(): HTMLElement {
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    root.append(node);
  }
  document.body.append(root);
  return root;
}

function objectName(value: unknown): unknown {
  return (value as Record<string, unknown>)["name"];
}

test("ordinary authorized action renders actual values as text and saves edited form", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body)) as Record<string, unknown>;
    calls.push({ path, body });
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description
      : path.endsWith("onchange") ? { value: { name: objectName(body["values"]) } }
      : path.endsWith("web_save") ? [{ id: 5, name: objectName(body["vals"]) }]
      : [{ id: 5, name: "<img src=x onerror=alert(1)>" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  const focus = vi.spyOn(HTMLElement.prototype, "focus");
  app = new HumanApp(root, bootstrap);
  await app.start();
  // the opened page takes focus without being scrolled to its own top edge: the menu bar above it stays in view
  const content = root.querySelector<HTMLElement>("[data-client-content]")!;
  expect(focus.mock.calls[focus.mock.contexts.lastIndexOf(content)]).toEqual([{ preventScroll: true }]);
  focus.mockRestore();
  expect([...root.querySelectorAll("img")].filter(img => !["/erp/brand/logo.svg", "/erp/brand/logo-dark.svg"].includes(img.getAttribute("src") ?? ""))).toEqual([]);
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  expect(input.value).toBe("<img src=x onerror=alert(1)>");
  input.value = "Changed";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(calls.some(call => call.path.endsWith("onchange"))).toBe(true));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(root.textContent).toContain("Record saved."));
  expect(calls.find(call => call.path.endsWith("web_save"))?.body["vals"]).toEqual({ name: "Changed" });
});

test("a session denial removes protected shell state before the error is displayed", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async () => new Response('{"message":"Expired"}', { status: 401 })));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await expect(app.start()).rejects.toMatchObject({ kind: "session" });
  expect(root.textContent).not.toContain("Operator");
  expect(root.querySelector("input[name=csrf_token]")).toBeNull();
  expect(root.querySelector("a")?.getAttribute("href")).toBe("/erp/login?redirect=%2Ferp");
  expect(root.textContent).not.toContain("Unsaved changes");
});

test("a session expiry with unsaved edits keeps the return path and says the edits were lost", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  let expired = false;
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    if (expired) return new Response('{"message":"Expired"}', { status: 401 });
    const body = JSON.parse(String(init.body)) as Record<string, unknown>;
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description
      : path.endsWith("onchange") ? { value: { name: objectName(body["values"]) } }
      : [{ id: 5, name: "Original" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Changed";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  expired = true;
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(root.querySelector("main h1")?.textContent).toBe("Session expired"));
  expect(root.textContent).not.toContain("Changed");
  expect(root.querySelector('[role="alert"]')?.textContent).toBe("Unsaved changes on this page were lost. Sign in, then enter them again.");
  const link = root.querySelector("a")!;
  expect(new URL(link.href).pathname).toBe("/erp/login");
  expect(new URL(link.href).searchParams.get("redirect")).toBe("/erp/action-20?id=5&view_type=form");
  // A second 401 from another in-flight request must not erase the warning or move the return path.
  history.replaceState(null, "", "/erp/action-99");
  (app as unknown as { expire(): void }).expire();
  expect(root.querySelectorAll("main")).toHaveLength(1);
  expect(root.querySelector('[role="alert"]')?.textContent).toBe("Unsaved changes on this page were lost. Sign in, then enter them again.");
  expect(root.querySelector("a")?.getAttribute("href")).toBe(link.getAttribute("href"));
});

test("a failing dirty check on session expiry is reported, the teardown still runs and the warning is shown", async () => {
  history.replaceState(null, "", "/erp");
  const reported = vi.fn();
  vi.stubGlobal("reportError", reported);
  const root = host();
  app = new HumanApp(root, bootstrap);
  const failure = new Error("dirty state unreadable");
  Object.defineProperty(app, "isDirty", { value: () => { throw failure; } });
  (app as unknown as { expire(): void }).expire();
  expect(reported).toHaveBeenCalledWith(failure);
  expect(root.querySelector("main h1")?.textContent).toBe("Session expired");
  expect(root.querySelector('[role="alert"]')?.textContent).toBe("Unsaved changes on this page were lost. Sign in, then enter them again.");
  expect(root.querySelector("a")?.getAttribute("href")).toBe("/erp/login?redirect=%2Ferp");
});

test("Back and Forward restore existing application history entries", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description : path.endsWith("default_get") ? { name: "New note" } : { value: { name: "New note" } };
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const originalLength = history.length;
  expect(root.querySelector('[aria-current="page"]')).toBeNull();
  [...root.querySelectorAll("button")].find(button => button.textContent === "Notes")!.click();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/erp/action-20"));
  expect(history.length).toBe(originalLength + 1);
  expect(root.querySelector('[aria-current="page"]')?.textContent).toBe("Notes");
  history.back();
  await vi.waitFor(() => expect(root.querySelector(".home-grid")).not.toBeNull());
  expect(root.querySelector('[aria-current="page"]')).toBeNull();
  expect(window.location.pathname).toBe("/erp");
  expect(history.length).toBe(originalLength + 1);
  history.forward();
  await vi.waitFor(() => expect(root.querySelector('input[name="name"]')).not.toBeNull());
  expect(root.querySelector('[aria-current="page"]')?.textContent).toBe("Notes");
  expect(window.location.pathname).toBe("/erp/action-20");
  expect(history.length).toBe(originalLength + 1);
});

test("rapid Back traversals share one dirty decision and restore the latest captured entry", async () => {
  history.replaceState(null, "", "/erp");
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description : path.endsWith("default_get") ? { name: "New note" } : { value: { name: "Changed" } };
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const click = (label: string): void => { [...root.querySelectorAll("button")].find(button => button.textContent === label)!.click(); };
  click("Notes");
  await vi.waitFor(() => expect(root.querySelector('input[name="name"]')).not.toBeNull());
  const brand = root.querySelector<HTMLButtonElement>('button.brand[aria-label="ERPForAll home"]')!;
  expect([...brand.querySelectorAll("img")].map(img => [img.getAttribute("src"), img.alt, img.className])).toEqual([
    ["/erp/brand/logo.svg", "ERPForAll", "brand-logo brand-logo-light"],
    ["/erp/brand/logo-dark.svg", "", "brand-logo brand-logo-dark"],
  ]);
  brand.click();
  await vi.waitFor(() => expect(root.querySelector(".home-grid")).not.toBeNull());
  click("Notes");
  await vi.waitFor(() => expect(root.querySelector('input[name="name"]')).not.toBeNull());
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Changed";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  history.back();
  await vi.waitFor(() => expect(document.querySelectorAll("dialog")).toHaveLength(1));
  history.back();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/erp/action-20"));
  expect(document.querySelectorAll("dialog")).toHaveLength(1);
  [...document.querySelectorAll("dialog button")].find(button => button.textContent === "Discard")!.dispatchEvent(new MouseEvent("click"));
  await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull());
  await vi.waitFor(() => expect(root.querySelector('input[name="name"]')).not.toBeNull());
  history.forward();
  await vi.waitFor(() => expect(root.querySelector(".home-grid")).not.toBeNull());
  vi.restoreAllMocks();
});

test("app switcher keeps labelled launcher buttons with decorative Lucide SVG icons", async () => {
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10, 11],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: "lucide:notebook-pen" },
        { module: null, id: 11, parent_id: null, label: "Settings", action_id: 21, children: [], icon: "lucide:settings" }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const overlay = root.querySelector<HTMLElement>(".home-menu-overlay")!;
  expect(overlay.classList.contains("hidden")).toBe(true);

  // Toggle app switcher
  const switcher = root.querySelector<HTMLButtonElement>("[data-app-switcher]")!;
  switcher.click();
  expect(overlay.classList.contains("hidden")).toBe(false);
  const content = root.querySelector<HTMLElement>("[data-client-content]")!;
  expect(content.classList.contains("home-menu-covered")).toBe(true);
  expect(content.inert).toBe(true);
  const navigation = root.querySelector<HTMLElement>("[data-client-navigation]")!;
  expect(navigation.classList.contains("home-menu-open")).toBe(true);
  expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(true);
  expect(overlay.style.top).toBe(`${Math.max(0, Math.round(root.querySelector<HTMLElement>("[data-client-header]")!.getBoundingClientRect().bottom))}px`);

  // SVGs are decorative; visible captions remain the accessible button names.
  const notesTile = overlay.querySelector('.app-tile[data-menu-id="10"]')!;
  expect(notesTile.querySelector('svg[data-icon="notebook-pen"]')?.getAttribute("aria-hidden")).toBe("true");
  expect(notesTile.textContent).toBe("Notes");
  const settingsTile = overlay.querySelector('.app-tile[data-menu-id="11"]')!;
  expect(settingsTile.querySelector('svg[data-icon="settings"]')?.getAttribute("focusable")).toBe("false");
  expect(settingsTile.textContent).toBe("Settings");
  expect(overlay.querySelector(".app-icon[data-letter]")).toBeNull();
  expect(switcher.querySelector('svg[data-icon="layout-grid"]')).not.toBeNull();

  // Toggle again closes overlay
  switcher.click();
  expect(overlay.classList.contains("hidden")).toBe(true);
  expect(content.classList.contains("home-menu-covered")).toBe(false);
  expect(content.inert).toBe(false);
  expect(navigation.classList.contains("home-menu-open")).toBe(false);
  expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(false);

  // Navigating while the menu is open (the brand button goes home) closes it and releases the page.
  switcher.click();
  expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(true);
  root.querySelector<HTMLButtonElement>("button.brand")!.click();
  await vi.waitFor(() => expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(false));
  const nextContent = root.querySelector<HTMLElement>("[data-client-content]")!;
  expect(nextContent.classList.contains("home-menu-covered")).toBe(false);
  expect(nextContent.inert).toBe(false);
  expect(root.querySelector<HTMLElement>(".home-menu-overlay")!.classList.contains("hidden")).toBe(true);
});

test("second-level menus appear under their root app rather than in a flat global row", async () => {
  history.replaceState(null, "", "/erp/action-21");
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    calls.push(path);
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10, 11],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
        { module: null, id: 11, parent_id: null, label: "Settings", action_id: 21, children: [101, 102], icon: null },
        { module: null, id: 101, parent_id: 11, label: "Users", action_id: 201, children: [], icon: null },
        { module: null, id: 102, parent_id: 11, label: "Companies", action_id: 202, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: body.ids?.[0] ?? 21,
      name: body.ids?.[0] === 201 ? "Users" : "Settings",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const sections = root.querySelector(".app-sections")!;
  expect(sections.textContent).toContain("Settings");
  expect(sections.textContent).toContain("Users");
  expect(sections.textContent).toContain("Companies");
  // Notes is NOT in the active secondary navbar sections
  expect(sections.querySelector('.nav-item[data-menu-id="10"]')).toBeNull();

  // Click Users sub-menu
  const usersBtn = [...sections.querySelectorAll("button")].find(b => b.textContent === "Users")!;
  usersBtn.click();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/erp/action-201"));
  expect(usersBtn.getAttribute("aria-current")).toBe("page");
});

test("breadcrumbs display action and record hierarchy with clickable back navigation", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 1, type: "form" }, { id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_read") ? [{ id: 5, name: "Meeting Agenda" }]
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Agenda" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const breadcrumbs = root.querySelector("[data-control-breadcrumbs]")!;
  expect(breadcrumbs.textContent).toContain("Notes");
  expect(breadcrumbs.textContent).toContain("Meeting Agenda");

  // Click back button to return to list view
  const backBtn = breadcrumbs.querySelector<HTMLButtonElement>(".breadcrumb-back")!;
  expect(backBtn.textContent).toBe("Notes");
  backBtn.click();
  await vi.waitFor(() => expect(window.location.search).toBe(""));
  await vi.waitFor(() => expect(root.querySelector("table[aria-label=Records]")).not.toBeNull());
});

test("search input creates removable facet chips and clears filter on remove", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchQueries: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      searchQueries.push(body.domain);
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Notes" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const searchInput = root.querySelector<HTMLInputElement>(".search-input")!;
  const searchForm = root.querySelector<HTMLFormElement>(".search-form")!;
  searchInput.value = "Meeting";
  searchForm.dispatchEvent(new Event("submit"));

  await vi.waitFor(() => expect(searchQueries.some(q => JSON.stringify(q).includes("Meeting"))).toBe(true));

  // Verify chip exists
  const chip = root.querySelector(".search-facet-chip")!;
  expect(chip.textContent).toContain('Name: "Meeting"');

  // Click chip remove button
  const removeBtn = chip.querySelector<HTMLButtonElement>(".chip-remove-btn")!;
  removeBtn.click();

  await vi.waitFor(() => expect(root.querySelector(".search-facet-chip")).toBeNull());
  await vi.waitFor(() => expect(searchQueries[searchQueries.length - 1]).toEqual([]));
});

/// (partial: live search cannot publish a canonical URL above the encoded URI cap)
test("search keeps the prior history URL and shows validation when Unicode facets exceed the encoded URI cap", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchQueries: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : {};
    if (path.endsWith("web_search_read")) searchQueries.push(body["domain"]);
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }], domain: null, context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Notes" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const input = root.querySelector<HTMLInputElement>(".search-input")!;
  const form = root.querySelector<HTMLFormElement>(".search-form")!;
  const first = "書".repeat(80), second = "名".repeat(170);
  input.value = first;
  form.dispatchEvent(new Event("submit"));
  await vi.waitFor(() => expect(searchQueries.some(query => JSON.stringify(query).includes(first))).toBe(true));
  const prior = location.pathname + location.search;
  expect(new TextEncoder().encode(prior).length).toBeLessThanOrEqual(2048);
  expect(new URL(location.href).searchParams.has("search_facets")).toBe(true);
  const published = vi.spyOn(history, "replaceState");

  // Each value fits SearchModel's 256-character bound, and their combined JSON is below 2048
  // characters. Percent-encoding the Unicode values is what exceeds the shell URI byte cap.
  input.value = second;
  form.dispatchEvent(new Event("submit"));
  expect(root.querySelector(".search-facet-chip")?.textContent).toContain(second);
  for (const call of published.mock.calls) {
    const url = new URL(String(call[2]), location.origin);
    expect(new TextEncoder().encode(url.pathname + url.search).length).toBeLessThanOrEqual(2048);
  }
  expect(location.pathname + location.search).toBe(prior);
  await vi.waitFor(() => expect(document.querySelector("#client-error-message")?.textContent).toBe("Invalid navigation parameter: url."));
});

test("search autocomplete allows selection and triggers filtered search", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchQueries: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      searchQueries.push(body.domain);
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Notes" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const searchInput = root.querySelector<HTMLInputElement>(".search-input")!;
  searchInput.value = "Project";
  searchInput.dispatchEvent(new Event("input"));

  const autocomplete = root.querySelector<HTMLElement>(".erp-search-autocomplete")!;
  expect(autocomplete.style.display).not.toBe("none");

  // Press Enter to select first autocomplete item
  searchInput.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));

  await vi.waitFor(() => expect(searchQueries.some(q => JSON.stringify(q).includes("Project"))).toBe(true));
  expect(root.querySelector(".search-facet-chip")?.textContent).toContain('Name: "Project"');
});

test("search menu panel toggles and adds custom filter to query domain", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchQueries: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      searchQueries.push(body.domain);
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Meeting Notes" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const toggleBtn = root.querySelector<HTMLButtonElement>(".search-menu-toggle")!;
  toggleBtn.click();

  const panel = root.querySelector<HTMLElement>(".erp-search-menu-panel")!;
  expect(panel.style.display).toBe("block");

  // Open custom filter form
  const addFilterBtn = panel.querySelector<HTMLButtonElement>(".erp-search-toggle-form-btn")!;
  addFilterBtn.click();

  const valInput = panel.querySelector<HTMLInputElement>(".erp-custom-val-input")!;
  valInput.value = "ActiveSprint";

  const form = panel.querySelector<HTMLFormElement>(".erp-custom-filter-form")!;
  form.dispatchEvent(new Event("submit"));

  await vi.waitFor(() => expect(searchQueries.some(q => JSON.stringify(q).includes("ActiveSprint"))).toBe(true));
});

test("systray user menu displays company and CSRF-protected logout", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const header = root.querySelector("[data-client-header]")!;
  // One top bar: the app navigation sits inside the header, between the brand and the systray.
  const navigation = root.querySelector("[data-client-navigation]")!;
  expect(navigation.parentElement).toBe(header);
  expect([...header.children].map(child => child === navigation ? "navigation" : child.classList[0])).toEqual(["brand", "navigation", "systray"]);
  const userBtn = header.querySelector<HTMLButtonElement>(".user-button")!;
  expect(userBtn.textContent).toContain("Operator");
  expect(userBtn.querySelector(".user-avatar")?.getAttribute("data-letter")).toBe("O");

  const dropdown = header.querySelector<HTMLElement>(".user-dropdown")!;
  expect(dropdown.classList.contains("open")).toBe(false);

  // Toggle user dropdown
  userBtn.click();
  expect(dropdown.classList.contains("open")).toBe(true);
  expect(dropdown.textContent).toContain("Operator");
  expect(header.querySelector<HTMLSelectElement>(".company-menu select")?.value).toBe("1");
  expect(header.querySelector(".company-menu-name")?.textContent).not.toBe("");
  const rowLabels = [...dropdown.querySelectorAll(".user-menu-row")].map(row => row.textContent ?? "");
  expect(rowLabels[0]).toContain("Shortcuts");
  expect(rowLabels.slice(1)).toEqual(["My preferences", "Sign out"]);

  const logoutForm = dropdown.querySelector<HTMLFormElement>("form")!;
  expect(logoutForm.action).toContain("/erp/session/logout");
  expect(logoutForm.querySelector<HTMLInputElement>("input[name=csrf_token]")?.value).toBe("test-token-not-secret");
});

test("list view header and row selection with count and delete action", async () => {
  history.replaceState(null, "", "/erp/action-20");
  let records = [{ id: 5, name: "Note A" }, { id: 6, name: "Note B" }];
  const unlinkCalls: number[][] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("unlink")) {
      unlinkCalls.push(body.ids);
      records = records.filter(r => !body.ids.includes(r.id));
      return new Response(JSON.stringify(true), { headers: { "Content-Type": "application/json" } });
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: records.length, records }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const table = root.querySelector("table.list-table")!;
  expect(table).not.toBeNull();
  expect(table.classList.contains("list-auto-layout")).toBe(false);
  expect((table as HTMLElement).style.tableLayout).toBe("fixed");

  const headerCheckbox = table.querySelector<HTMLInputElement>(".list-select-all")!;
  const rowCheckboxes = table.querySelectorAll<HTMLInputElement>(".list-row-select");
  expect(rowCheckboxes.length).toBe(2);

  const selectionBox = root.querySelector<HTMLElement>(".selection-box")!;
  expect(selectionBox.style.display).toBe("none");

  // Padding is part of the selection target, never a record-navigation target.
  rowCheckboxes[0]!.parentElement!.click();
  expect(rowCheckboxes[0]!.checked).toBe(true);
  expect(location.search).not.toContain("id=");
  rowCheckboxes[0]!.click();
  expect(rowCheckboxes[0]!.checked).toBe(false);
  rowCheckboxes[0]!.parentElement!.click();
  expect(selectionBox.style.display).not.toBe("none");
  expect(root.querySelector(".selection-count")?.textContent).toBe("1 selected");
  expect(headerCheckbox.indeterminate).toBe(true);

  // Toggle header checkbox to select all
  headerCheckbox.checked = true;
  headerCheckbox.dispatchEvent(new Event("change"));
  expect(root.querySelector(".selection-count")?.textContent).toBe("2 selected");

  // Open action dropdown and click Delete
  const actionToggle = root.querySelector<HTMLButtonElement>(".action-toggle-btn")!;
  actionToggle.click();

  // Dialog auto-confirm
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", {
    configurable: true,
    value: function (this: HTMLDialogElement) {
      this.open = true;
      const primaryBtn = this.querySelector<HTMLButtonElement>("button[data-default-action]");
      setTimeout(() => primaryBtn?.click(), 10);
    }
  });
  Object.defineProperty(HTMLDialogElement.prototype, "close", {
    configurable: true,
    value: function (this: HTMLDialogElement) {
      this.open = false;
    }
  });

  const deleteBtn = root.querySelector<HTMLButtonElement>(".action-delete-btn")!;
  deleteBtn.click();

  await vi.waitFor(() => expect(unlinkCalls.length).toBe(1));
  expect(unlinkCalls[0]!.sort()).toEqual([5, 6]);
});

test("list columns settle synchronously on initial load and reload (no auto-layout frame)", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const records = [{ id: 5, name: "Note A" }, { id: 6, name: "Note B" }];
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: records.length, records }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const table = root.querySelector<HTMLTableElement>("table.list-table")!;
  expect(table).not.toBeNull();
  // Settle must happen synchronously upon attachment to the DOM in start()/reload:
  // table-layout must be fixed, list-auto-layout must be removed, and col widths set.
  expect(table.classList.contains("list-auto-layout")).toBe(false);
  expect(table.style.tableLayout).toBe("fixed");
  expect(table.querySelectorAll("col").length).toBeGreaterThan(1);
  // every column but the last has its pixel width; the last takes the spare room, so it has none.
  const cols = Array.from(table.querySelectorAll("col"));
  for (const col of cols.slice(0, -1)) expect(col.style.width).toMatch(/^[0-9]+px$/);
  expect(cols.at(-1)!.style.width).toBe("");
});

/// (partial: Office Blue list hierarchy and real pagination controller)
test("Office Blue list has a title, metadata ancestry and bounded real page navigation", async () => {
  document.documentElement.dataset["officeTheme"] = "office-blue";
  history.replaceState(null, "", "/erp/action-20?menu=10");
  const reads: { offset: number; limit: number }[] = [];
  const domains: unknown[] = [];
  const activeDescription = { ...description, fields: { "test.note": { ...description.fields["test.note"], active: { type: "boolean", label: "Active", required: false, readonly: false, relation: null, relation_field: null, selection: null } } } };
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [9], nodes: [
        { module: null, id: 9, parent_id: null, children: [10], label: "Workspace", action_id: null, icon: null },
        { module: null, id: 10, parent_id: 9, children: [], label: "Notes", action_id: 20, icon: null }
      ]
    } : path.endsWith("erp_load_web_action_views") ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
    }, activeDescription) : path.endsWith("web_search_read") ? (() => {
      reads.push({ offset: body.offset, limit: body.limit });
      domains.push(body.domain);
      if (body.domain.length) return { length: 0, records: [] };
      return { length: 240, records: Array.from({ length: body.limit }, (_, index) => ({ id: body.offset + index + 1, name: `Note ${body.offset + index + 1}` })) };
    })() : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  try {
    const root = host(); app = new HumanApp(root, bootstrap); await app.start();
    expect(root.querySelector(".control-panel-list h1")?.textContent).toBe("Notes");
    expect(root.querySelector<HTMLHeadingElement>(".control-panel-list h1")?.title).toBe("Workspace › Notes");
    const footer = root.querySelector(".control-panel-list .pager")!;
    expect(root.querySelector(".list-footer")).toBeNull();
    expect(footer).not.toBeNull();
    expect(footer.querySelector(".pager-count")?.textContent).toBe("1–40 / 240");
    expect(footer.querySelector('[aria-current="page"]')?.textContent).toBe("1");
    const options = footer.querySelector<HTMLButtonElement>(".pager-options")!;
    options.dispatchEvent(new KeyboardEvent("keydown", {key:"ArrowDown",bubbles:true}));
    footer.querySelector('.pager-menu')!.dispatchEvent(new KeyboardEvent("keydown", {key:"End",bubbles:true}));
    const sizeTrigger = footer.querySelector<HTMLButtonElement>('.list-page-size .choice-dropdown-trigger')!;
    expect(document.activeElement).toBe(sizeTrigger);
    expect(sizeTrigger.getAttribute("role")).toBe("menuitem"); // review N5: an item of role=menu, not a plain button
    sizeTrigger.dispatchEvent(new KeyboardEvent("keydown", {key:"ArrowDown",bubbles:true}));
    expect(document.activeElement).toBe(footer.querySelector('.list-page-size [data-value="20"]'));
    document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", {key:"ArrowDown",bubbles:true}));
    expect(document.activeElement).toBe(footer.querySelector('.list-page-size [data-value="40"]'));
    document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", {key:"Escape",bubbles:true}));
    expect(document.activeElement).toBe(sizeTrigger);
    expect(options.getAttribute("aria-expanded")).toBe("true");
    sizeTrigger.dispatchEvent(new KeyboardEvent("keydown", {key:"Escape",bubbles:true}));
    expect(options.getAttribute("aria-expanded")).toBe("false");

    (footer.querySelector('[aria-label="Page 6"]') as HTMLButtonElement).click();
    await vi.waitFor(() => expect(reads.at(-1)).toEqual({ offset: 200, limit: 40 }));
    await vi.waitFor(() => expect(footer.querySelector(".pager-count")?.textContent).toBe("201–240 / 240"));
    expect(footer.querySelector<HTMLButtonElement>(".pager-next")?.disabled).toBe(true);
    expect(footer.querySelector('[aria-current="page"]')?.textContent).toBe("6");
    expect(footer.querySelectorAll(".pager-page").length).toBeLessThanOrEqual(5);
    (footer.querySelector(".pager-prev") as HTMLButtonElement).click();
    await vi.waitFor(() => expect(reads.at(-1)).toEqual({ offset: 160, limit: 40 }));
    expect(root.querySelectorAll(".list-row-select")).toHaveLength(40);
    footer.querySelector<HTMLButtonElement>('.list-page-size [data-value="50"]')!.click();
    await vi.waitFor(() => expect(reads.at(-1)).toEqual({ offset: 0, limit: 50 }));
    await vi.waitFor(() => expect(footer.querySelector(".pager-count")?.textContent).toBe("1–50 / 240"));
    expect(root.querySelectorAll(".list-row-select")).toHaveLength(50);
    const active = root.querySelector<HTMLElement>(".list-active-filter")!;
    expect(active).not.toBeNull();
    active.querySelector<HTMLButtonElement>('[data-value="archived"]')!.click();
    await vi.waitFor(() => expect(root.querySelector(".erp-facet-filter")?.textContent).toContain("Archived"));
    await vi.waitFor(() => expect(footer.querySelector(".pager-count")?.textContent).toBe("0 records"));
    expect(domains.at(-1)).toEqual([["active", "=", false]]);
    active.querySelector<HTMLButtonElement>('[data-value="all"]')!.click();
    await vi.waitFor(() => expect(footer.querySelector(".pager-count")?.textContent).toBe("1–50 / 240"));
    expect(domains.at(-1)).toEqual([]);
    const search = root.querySelector<HTMLInputElement>(".search-input")!;
    search.value = "missing";
    root.querySelector<HTMLFormElement>(".search-form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await vi.waitFor(() => expect(footer.querySelector(".pager-count")?.textContent).toBe("0 records"));
    expect(footer.querySelectorAll(".pager-page")).toHaveLength(0);
    expect(footer.querySelector<HTMLButtonElement>(".pager-prev")?.disabled).toBe(true);
    expect(footer.querySelector<HTMLButtonElement>(".pager-next")?.disabled).toBe(true);
  } finally { delete document.documentElement.dataset["officeTheme"]; }
});

/// (partial: real controller grouped domains, counts, filtering, page size and stale reads)
test("declared list tabs count the base search, filter actual rows and ignore a stale failed count", async () => {
  document.documentElement.dataset["officeTheme"] = "office-blue";
  history.replaceState(null, "", "/erp/action-20");
  const records = [{ id: 1, name: "match normal", priority: "normal" }, { id: 2, name: "match high", priority: "high" }, { id: 3, name: "other unset", priority: false as const }];
  const tabDescription = { ...description, fields: { "test.note": { ...description.fields["test.note"], priority: { type: "selection", label: "Priority", required: false, readonly: false, relation: null, relation_field: null, selection: [["normal", "Normal"], ["high", "High"]] } } }, views: description.views.map(view => view.type === "list" ? { ...view, nodes: [{ kind: "list", children: [1], page_limit: 250, search_panel: { kind: "selection", field: { model: "test.note", name: "priority" }, label: "Priority", enable_counters: true, expand: true, default_value: null, notice: null } }, view.nodes[1]] } : view) };
  const rowDomains: unknown[][] = [], countDomains: unknown[][] = [], limits: number[] = [];
  let deferCount = false, failCount = false, releaseCount: (() => void) | undefined;
  const matching = (domain: unknown[][]) => records.filter(row => domain.every(([field, operator, value]) => operator === "ilike" ? row.name.includes(String(value)) : row[field as "priority"] === value));
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    let value: unknown;
    if (path.endsWith("erp_load_web_menus")) value = { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, children: [], label: "Notes", action_id: 20, icon: null }] };
    else if (path.endsWith("erp_load_web_action_views")) value = actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null }, tabDescription);
    else if (path.endsWith("web_search_read")) { rowDomains.push(body.domain); limits.push(body.limit); const rows = matching(body.domain); value = { length: rows.length, records: rows.slice(body.offset, body.offset + body.limit) }; }
    else if (path.endsWith("formatted_read_group")) {
      countDomains.push(body.domain);
      if (failCount) { failCount = false; return new Response(JSON.stringify({ message: "Current count failed" }), { status: 500, headers: { "Content-Type": "application/json" } }); }
      expect(body.groupby).toEqual(["priority"]); expect(body.aggregates).toEqual(["__count"]);
      expect(body.context.allowed_company_ids).toEqual([1]);
      if (deferCount) { deferCount = false; await new Promise<void>(resolve => { releaseCount = resolve; }); return new Response(JSON.stringify({ message: "Stale count failed" }), { status: 500, headers: { "Content-Type": "application/json" } }); }
      const counts = new Map<string | false, number>(); for (const row of matching(body.domain)) counts.set(row.priority, (counts.get(row.priority) ?? 0) + 1);
      value = [...counts].map(([priority, count]) => ({ priority, __count: count }));
    } else value = [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  try {
    const root = host(); app = new HumanApp(root, bootstrap); await app.start();
    expect(limits[0]).toBe(250);
    expect(root.querySelector<HTMLButtonElement>(".list-page-size .choice-dropdown-trigger")?.value).toBe("250");
    expect(root.querySelector('.list-page-size [data-value="250"]')).not.toBeNull();
    const tabs = root.querySelector(".list-state-tabs")!;
    expect(tabs.querySelector('[data-tab-token="all"]')?.textContent).toBe("All3");
    const high = tabs.querySelector<HTMLButtonElement>('[data-tab-token="value:high"]')!;
    high.click();
    await vi.waitFor(() => expect(root.querySelectorAll(".list-row-select")).toHaveLength(1));
    expect(rowDomains.at(-1)).toEqual([["priority", "=", "high"]]);
    expect(countDomains.at(-1)).toEqual([]);
    expect(location.search).toContain("list_tab=value%3Ahigh");
    expect(tabs.querySelector('[data-tab-token="all"]')?.textContent).toBe("All3");
    const search = root.querySelector<HTMLInputElement>(".search-input")!; search.value = "match";
    root.querySelector(".search-form")!.dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
    await vi.waitFor(() => expect(tabs.querySelector('[data-tab-token="all"]')?.textContent).toBe("All2"));
    expect(countDomains.at(-1)).toEqual([["name", "ilike", "match"]]);
    expect(rowDomains.at(-1)).toEqual([["name", "ilike", "match"], ["priority", "=", "high"]]);
    root.querySelector<HTMLButtonElement>('.list-page-size [data-value="50"]')!.click();
    await vi.waitFor(() => expect(limits.at(-1)).toBe(50));
    expect(rowDomains.at(-1)).toEqual([["name", "ilike", "match"], ["priority", "=", "high"]]);
    tabs.querySelector<HTMLButtonElement>('[data-tab-token="value:normal"]')!.click();
    await vi.waitFor(() => expect(root.querySelector(".list-table tbody button")?.textContent).toBe("match normal"));
    deferCount = true; high.click();
    await vi.waitFor(() => expect(releaseCount).toBeTypeOf("function"));
    tabs.querySelector<HTMLButtonElement>('[data-tab-token="value:normal"]')!.click();
    await vi.waitFor(() => expect(tabs.getAttribute("aria-busy")).toBe("false"));
    releaseCount!();
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(root.querySelector(".list-table tbody button")?.textContent).toBe("match normal");
    expect(tabs.querySelector('[aria-selected="true"]')?.textContent).toBe("Normal1");
    expect(document.querySelector(".client-error-dialog")).toBeNull();
    failCount = true; high.click();
    await vi.waitFor(() => expect(root.querySelector(".list-load-error")?.textContent).toContain("Unable to load"));
    expect(root.querySelector(".list-table")).toBeNull();
    expect(root.querySelector(".pager-count")?.textContent).toBe("Results unavailable");
    expect(root.querySelector<HTMLButtonElement>(".pager-prev")?.disabled).toBe(true);
    expect(root.querySelector<HTMLButtonElement>(".pager-next")?.disabled).toBe(true);
    expect(root.querySelector(".pager-pages [aria-current]")).toBeNull();
    expect(tabs.querySelector('[data-tab-token="all"] .list-tab-count')?.textContent).toBe("—");
    expect(document.querySelector(".client-error-dialog")).not.toBeNull();
    document.querySelector<HTMLButtonElement>("[data-error-close]")!.click();
    tabs.querySelector<HTMLButtonElement>('[data-tab-token="value:normal"]')!.click();
    await vi.waitFor(() => expect(root.querySelector(".list-table tbody button")?.textContent).toBe("match normal"));
    releaseCount = undefined; deferCount = true; high.click();
    await vi.waitFor(() => expect(releaseCount).toBeTypeOf("function"));
    root.querySelector<HTMLButtonElement>(".brand")!.click();
    await vi.waitFor(() => expect(root.querySelector(".home-grid")).not.toBeNull());
    releaseCount!();
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(root.querySelector(".home-grid")).not.toBeNull();
    expect(root.querySelector(".list-table")).toBeNull();
    expect(document.querySelector(".client-error-dialog")).toBeNull();
  } finally { releaseCount?.(); delete document.documentElement.dataset["officeTheme"]; }
});

test("list view column sort with direction indicator and row click opens form", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchCalls: { order?: string }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      searchCalls.push(body);
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Note A" }] }
      : path.endsWith("web_read") ? [{ id: 5, name: "Note A" }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const sortBtn = root.querySelector<HTMLButtonElement>(".sort-button")!;
  expect(sortBtn.textContent).toContain("Name");

  // Click sort to toggle to name asc
  sortBtn.click();
  await vi.waitFor(() => expect(searchCalls.some(c => c.order === "name asc")).toBe(true));
  await vi.waitFor(() => expect(root.querySelector(".sort-button .sort-indicator")).not.toBeNull());
  expect(root.querySelector("th.sortable-column")?.getAttribute("aria-sort")).toBe("ascending");

  // Click again to toggle to name desc
  root.querySelector<HTMLButtonElement>(".sort-button")!.click();
  await vi.waitFor(() => expect(searchCalls.some(c => c.order === "name desc")).toBe(true));
  await vi.waitFor(() => expect(root.querySelector("th.sortable-column")?.getAttribute("aria-sort")).toBe("descending"));
  expect(root.querySelector(".sort-button .sort-indicator")).not.toBeNull();
  expect(root.querySelector(".sort-button")?.textContent).toBe("Name");

  // Click row to navigate to form
  const row = root.querySelector<HTMLElement>("tbody tr")!;
  row.click();
  await vi.waitFor(() => expect(window.location.search).toContain("id=5"));
  await vi.waitFor(() => expect(root.querySelector(".content-sheet")).not.toBeNull());
});

test.each([false, true])("form state and Save/Discard retain validation with Office presentation %s", async office => {
  if (office) document.documentElement.dataset["officeTheme"] = "office-blue";
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const saveCalls: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_save")) {
      saveCalls.push(body);
    }
    const value = path.endsWith("get_res_users_settings") ? { homemenu_config: null } : path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 1, type: "form" }, { id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_read") ? [{ id: 5, name: "Existing note" }]
      : path.endsWith("onchange") ? { value: { name: body.values?.name } }
      : path.endsWith("web_save") ? [{ id: 5, name: body.vals?.name }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  // The route mounts staging children, so the persistent viewport must wrap the sheet itself.
  expect(root.querySelector(".record-form-viewport > .content-sheet > .record-form")).not.toBeNull();
  expect(root.querySelector(".record-form-viewport [data-control-panel]")).toBeNull();
  const breadcrumbs = root.querySelector("[data-control-breadcrumbs]")!;
  const statusHost = breadcrumbs;
  expect(statusHost).not.toBeNull();
  const statusIndicator = statusHost.querySelector(".form-status-indicator")!;
  expect(statusIndicator).not.toBeNull();
  expect(root.querySelectorAll(".form-status-indicator")).toHaveLength(1);
  expect(breadcrumbs.querySelector(".form-status-indicator")).not.toBeNull();

  const statusBadge = statusIndicator.querySelector(".status-badge")!;
  expect(statusBadge.textContent).toBe("Saved");
  expect(statusBadge.classList.contains("saved")).toBe(true);
  expect(root.querySelector('.client-notification[data-level="danger"]')).toBeNull();

  // Status actions (Save/Discard) initially hidden when clean
  const statusActions = statusIndicator.querySelector<HTMLElement>(".status-actions")!;
  expect(statusActions.style.display).toBe("none");

  // Modify input
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  if (office) {
    expect(root.querySelector(".record-form input[name=name]")).toBe(input);
    expect(root.querySelector(".control-panel input[name=name]")).toBeNull();
    expect(root.querySelectorAll("input[name=name]")).toHaveLength(1);
    expect(input.form).toBe(root.querySelector(".record-form"));
  }
  input.value = "Updated note";
  input.dispatchEvent(new Event("change"));

  await vi.waitFor(() => expect(statusBadge.textContent).toBe("Unsaved changes"));
  expect(statusBadge.classList.contains("dirty")).toBe(true);
  expect(statusActions.style.display).not.toBe("none");
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));

  // Click Save button in status indicator
  const saveBtn = root.querySelector<HTMLButtonElement>(".status-btn-save")!;
  expect(saveBtn.textContent).toBe("Save");
  {
    const externalTitle = root.querySelector<HTMLInputElement>('.record-form input[name="name"]')!;
    externalTitle.value = "";
    saveBtn.click();
    await vi.waitFor(() => expect(document.activeElement).toBe(externalTitle));
    expect(saveCalls).toHaveLength(0);
    expect(root.querySelector('.client-notification[data-level="warning"] .client-notification-message')?.textContent).toBe("1 field needs a value");
    expect(externalTitle.getAttribute("aria-invalid")).toBe("true");
    externalTitle.value = "Updated note";
  }
  saveBtn.click();

  await vi.waitFor(() => expect(saveCalls.length).toBe(1));
  await vi.waitFor(() => expect(root.querySelector(".status-badge")?.textContent).toBe("Saved"));
  if (office) expect(document.activeElement).toBe(root.querySelector('.record-form input[name="name"]'));
  expect(root.querySelector<HTMLElement>(".status-actions")?.style.display).toBe("none");
});

// It lives in the Actions menu (one click), asks first,
// saves a dirty record before it acts, and an archived record says so quietly in the control panel.
test("form Archive and Unarchive live in the Actions menu and the archived state is visible", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const calls: { method: string; ids: number[] }[] = [];
  let active = true;
  const note = description.fields["test.note"];
  const archivable = {
    ...description, fields: { "test.note": { ...note, active: { type: "boolean", label: "Active", required: false, readonly: false, relation: null, relation_field: null, selection: null } } },
    views: description.views.map(view => view.type !== "form" ? view : {
      ...view, nodes: [{ kind: "form", record_mode: "record", children: [1, 2] }, view.nodes[1],
        { kind: "field", force_save: false, field: { model: "test.note", name: "active" }, widget: "boolean_toggle", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Active" }]
    })
  };
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const method = path.split("/").pop()!;
    if (method === "action_archive" || method === "action_unarchive") { calls.push({ method, ids: body.ids }); active = method === "action_unarchive"; }
    const value = path.endsWith("get_res_users_settings") ? { homemenu_config: null } : path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }, { id: 2, type: "list" }], domain: null, context: null
    }, archivable) : path.endsWith("get_views") ? archivable
      : path.endsWith("web_read") ? [{ id: 5, name: "Existing note", active }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  await vi.waitFor(() => expect(root.querySelector(".form-actions-dropdown")).not.toBeNull());
  const menuItems = (): string[] => [...root.querySelectorAll<HTMLButtonElement>(".form-action-menu button")].filter(item => !item.hidden).map(item => item.textContent!);
  expect(menuItems()).toEqual(["Duplicate", "Archive", "Delete", "Reload"]);
  expect(root.querySelector<HTMLElement>(".status-badge.archived")!.hidden).toBe(true);

  const confirm = async (label: string): Promise<void> => {
    await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull());
    [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(item => item.textContent === label)!.click();
  };
  root.querySelector<HTMLButtonElement>(".form-archive-btn")!.click();
  await confirm("Cancel");
  await Promise.resolve();
  expect(calls).toEqual([]);

  root.querySelector<HTMLButtonElement>(".form-archive-btn")!.click();
  await confirm("Archive");
  await vi.waitFor(() => expect(calls).toEqual([{ method: "action_archive", ids: [5] }]));
  await vi.waitFor(() => expect(root.querySelector<HTMLElement>(".status-badge.archived")!.hidden).toBe(false));
  expect(root.querySelector(".status-badge.archived")!.textContent).toBe("Archived");
  expect(menuItems()).toEqual(["Duplicate", "Unarchive", "Delete", "Reload"]);

  // Unarchiving is harmless, so it does not ask.
  root.querySelector<HTMLButtonElement>(".form-archive-btn")!.click();
  await vi.waitFor(() => expect(calls.map(call => call.method)).toEqual(["action_archive", "action_unarchive"]));
  await vi.waitFor(() => expect(root.querySelector<HTMLElement>(".status-badge.archived")!.hidden).toBe(true));
  expect(menuItems()).toEqual(["Duplicate", "Archive", "Delete", "Reload"]);
});

test("form view record pager cycles between records", async () => {
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 2, records: [{ id: 5, name: "Note A" }, { id: 6, name: "Note B" }] }
      : path.endsWith("web_read") ? [{ id: body.ids?.[0] ?? 5, name: body.ids?.[0] === 6 ? "Note B" : "Note A" }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  // Click first row to open form
  const firstRow = root.querySelector<HTMLElement>("tbody tr[data-id='5']")!;
  firstRow.click();

  await vi.waitFor(() => expect(root.querySelector(".form-pager")).not.toBeNull());
  const pager = root.querySelector(".form-pager")!;
  const pagerCount = pager.querySelector(".pager-count")!;
  expect(pagerCount.textContent).toBe("1 / 2");

  const prevBtn = pager.querySelector<HTMLButtonElement>(".pager-prev")!;
  const nextBtn = pager.querySelector<HTMLButtonElement>(".pager-next")!;
  expect(prevBtn.disabled).toBe(true);
  expect(nextBtn.disabled).toBe(false);

  // Click next record
  nextBtn.click();
  await vi.waitFor(() => expect(window.location.search).toContain("id=6"));
  await vi.waitFor(() => expect(root.querySelector(".pager-count")?.textContent).toBe("2 / 2"));
  expect(root.querySelector<HTMLButtonElement>(".pager-prev")?.disabled).toBe(false);
  expect(root.querySelector<HTMLButtonElement>(".pager-next")?.disabled).toBe(true);
});

function pagerSaveFetch(log: { saved: unknown[]; reads: number[] }, fail: boolean): (path: string, init: RequestInit) => Promise<Response> {
  return vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_save")) {
      log.saved.push(body.vals);
      if (fail) return new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Name is taken." }), { status: 422, headers: { "Content-Type": "application/json" } });
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 2, records: [{ id: 5, name: "Note A" }, { id: 6, name: "Note B" }] }
      : path.endsWith("onchange") ? { value: {} }
      : path.endsWith("web_save") ? [{ id: 5, name: "Changed" }]
      : path.endsWith("web_read") ? (log.reads.push(body.ids[0]), [{ id: body.ids[0], name: body.ids[0] === 6 ? "Note B" : "Note A" }])
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  });
}

async function editFirstNote(root: HTMLElement): Promise<void> {
  root.querySelector<HTMLElement>("tbody tr[data-id='5']")!.click();
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("1 / 2"));
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Changed";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
}

test("form pager saves unsaved changes before moving, without asking", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, false));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await editFirstNote(root);
  root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("2 / 2"));
  expect(log.saved).toEqual([{ name: "Changed" }]);
  expect(document.querySelector("dialog")).toBeNull();
  expect(window.location.search).toContain("id=6");
});

// A failed save keeps the record and its edits in place; the pager does not move.
test("form pager stays on the record when the save fails", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, true));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await editFirstNote(root);
  root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!.click();
  await vi.waitFor(() => expect(log.saved).toHaveLength(1));
  await new Promise(resolve => setTimeout(resolve, 50));
  expect(log.reads).not.toContain(6);
  expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("1 / 2");
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed");
});

function pagedFetch(total: number, log: { unlink: number[][]; offsets: number[] }): (path: string, init: RequestInit) => Promise<Response> {
  return vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? (() => {
        log.offsets.push(body.offset);
        const ids = Array.from({ length: Math.max(0, Math.min(40, total - body.offset)) }, (_, i) => body.offset + i + 1);
        return { length: total, records: ids.map(id => ({ id, name: `Note ${id}` })) };
      })()
      : path.endsWith("unlink") ? (log.unlink.push(body.ids), true)
      : path.endsWith("web_read") ? [{ id: body.ids[0], name: `Note ${body.ids[0]}` }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  });
}

test("form pager moves across the page edge with 41 records", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { unlink: [] as number[][], offsets: [] as number[] };
  vi.stubGlobal("fetch", pagedFetch(41, log));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  root.querySelector<HTMLElement>("tbody tr[data-id='40']")!.click();
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("40 / 41"));
  const next = root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!;
  expect(next.disabled).toBe(false);
  next.click();
  await vi.waitFor(() => expect(window.location.search).toContain("id=41"));
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("41 / 41"));
  expect(window.location.search).not.toContain("undefined");
  expect(log.offsets).toContain(40);
  expect(root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!.disabled).toBe(true);

  root.querySelector<HTMLButtonElement>(".form-pager .pager-prev")!.click();
  await vi.waitFor(() => expect(window.location.search).toContain("id=40"));
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("40 / 41"));
  expect(window.location.search).not.toContain("undefined");
});

test("form pager reports an empty adjacent page without leaving the current record", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { unlink: [] as number[][], offsets: [] as number[] };
  const fetchPage = pagedFetch(41, log);
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read") && body.offset === 40) {
      return new Response(JSON.stringify({ length: 41, records: [] }), { headers: { "Content-Type": "application/json" } });
    }
    return fetchPage(path, init);
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  root.querySelector<HTMLElement>("tbody tr[data-id='40']")!.click();
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("40 / 41"));

  const errors: unknown[] = [];
  const run = app.run;
  app.run = job => run(async () => {
    try { await job(); } catch (error) { errors.push(error); throw error; }
  });
  root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!.click();
  await vi.waitFor(() => expect(errors).toHaveLength(1));
  expect(errors[0]).toMatchObject({ kind: "server", message: "The adjacent page returned no records." });
  expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("The adjacent page returned no records.");
  expect(new URLSearchParams(window.location.search).get("id")).toBe("40");
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Note 40");
  expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("40 / 41");
});

test("list selection is cleared on page change so Delete cannot reach hidden records", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { unlink: [] as number[][], offsets: [] as number[] };
  vi.stubGlobal("fetch", pagedFetch(41, log));
  const removed = vi.spyOn(document, "removeEventListener");
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const first = root.querySelector<HTMLInputElement>("tbody tr[data-id='1'] input.list-row-select")!;
  first.checked = true;
  first.dispatchEvent(new Event("change"));
  expect(root.querySelector(".selection-count")?.textContent).toBe("1 selected");

  root.querySelector<HTMLButtonElement>(".pager-next")!.click();
  await vi.waitFor(() => expect(root.querySelector("tbody tr[data-id='41']")).not.toBeNull());
  expect(root.querySelector(".selection-count")?.textContent).toBe("0 selected");
  expect(root.querySelector<HTMLElement>(".selection-box")!.style.display).toBe("none");

  const only = root.querySelector<HTMLInputElement>("tbody tr[data-id='41'] input.list-row-select")!;
  only.checked = true;
  only.dispatchEvent(new Event("change"));
  expect(root.querySelector(".selection-count")?.textContent).toBe("1 selected");
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", {
    configurable: true,
    value: function (this: HTMLDialogElement) {
      this.open = true;
      setTimeout(() => this.querySelector<HTMLButtonElement>("button[data-default-action]")?.click(), 10);
    }
  });
  Object.defineProperty(HTMLDialogElement.prototype, "close", {
    configurable: true,
    value: function (this: HTMLDialogElement) { this.open = false; }
  });
  root.querySelector<HTMLButtonElement>(".action-delete-btn")!.click();
  await vi.waitFor(() => expect(log.unlink.length).toBe(1));
  expect(log.unlink[0]).toEqual([41]);
  await vi.waitFor(() => expect(root.querySelector(".selection-count")?.textContent).toBe("0 selected"));

  const before = removed.mock.calls.filter(call => call[0] === "click").length;
  app.dispose();
  expect(removed.mock.calls.filter(call => call[0] === "click").length).toBeGreaterThan(before);
  removed.mockRestore();
});

test("related-record opening uses an advertised form action and preserves model routing", async () => {
  history.replaceState(null,"","/erp/action-20?id=5&view_type=form");
  const source=structuredClone(description) as {fields:Record<string,Record<string,unknown>>;views:{nodes:Record<string,unknown>[]}[]};
  Object.assign(source.fields["test.note"]!,{company_id:{type:"many2one",label:"Company",required:false,readonly:true,relation:"res.company", relation_field: null,selection:null}});
  const form=source.views[0]!;
  (form.nodes[0]!["children"] as number[]).push(2);
  form.nodes.push({kind:"field", force_save: false,field:{model:"test.note",name:"company_id"},widget:"many2one",options:{},modifiers:{readonly:null,required:null,invisible:null},label:"Company"});
  const related=JSON.parse(JSON.stringify(description).replaceAll("test.note","res.company"));
  const calls:string[]=[];
  vi.stubGlobal("fetch",vi.fn(async(path:string,init:RequestInit)=>{
    calls.push(path);const body=JSON.parse(String(init.body));let value:unknown;
    if(path.endsWith("erp_load_web_menus"))value={wire_major:1,registry_version:"r1",roots:[10,12,11],nodes:[{ module: null,id:10,parent_id:null,label:"Notes",action_id:20,children:[],icon:null},{ module: null,id:12,parent_id:null,label:"Unrelated",action_id:22,children:[],icon:null},{ module: null,id:11,parent_id:null,label:"Companies",action_id:21,children:[],icon:null}]};
    else if(path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")){
      const id=body.ids[0];
      // Discovery must not describe this unrelated model's inaccessible views.
      if(id===22 && path.endsWith("erp_load_web_action_views")) return new Response(JSON.stringify({message:"Unrelated view denied."}),{status:403,headers:{"Content-Type":"application/json"}});
      value=actionMetadata(path,{wire_major:1,id,name:id===20?"Notes":"Companies",model:id===20?"test.note":id===22?"denied.note":"res.company",target:"current",views:[{id:1,type:"form"}],domain:null,context:null},id===20?source:related);
    }
    else if(path.endsWith("get_views"))value=path.includes("res.company")?related:source;
    else value=path.includes("res.company")?[{id:9,name:"Company record"}]:[{id:5,name:"Source record",company_id:{id:9,display_name:"Company record"}}];
    return new Response(JSON.stringify(value),{headers:{"Content-Type":"application/json"}});
  }));
  const root=host();app=new HumanApp(root,bootstrap);await app.start();
  root.querySelector<HTMLButtonElement>('[aria-label="Open Company: Company record"]')!.click();
  await vi.waitFor(()=>expect(location.pathname).toBe("/erp/action-21"));
  expect(new URLSearchParams(location.search).get("model")).toBe("res.company");
  expect(new URLSearchParams(location.search).get("id")).toBe("9");
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')!.value).toBe("Company record");
  expect(calls.some(path=>path.endsWith("erp_load_web_action"))).toBe(true);
  expect(calls.filter(path=>path.endsWith("erp_load_web_action_views"))).toHaveLength(2);
  expect(calls.some(path=>path.includes("res.company")&&path.endsWith("get_views"))).toBe(false);
});

test("failed initial list load disposes search document listeners", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const added = vi.spyOn(document, "addEventListener"), removed = vi.spyOn(document, "removeEventListener");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    if (path.endsWith("web_search_read")) return new Response(JSON.stringify({ message: "List unavailable." }), { status: 500 });
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [], nodes: [] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description : null;
    return new Response(JSON.stringify(value), { status: 200 });
  }));
  const root = host(); app = new HumanApp(root, bootstrap);
  added.mockClear(); removed.mockClear();
  await expect(app.start()).rejects.toMatchObject({ kind: "server", message: "List unavailable." });
  const clickListeners = added.mock.calls.filter(([type]) => type === "click").map(([, listener]) => listener);
  expect(clickListeners.length).toBeGreaterThan(0);
  for (const listener of clickListeners) expect(removed.mock.calls.some(([type, fn]) => type === "click" && fn === listener)).toBe(true);
  added.mockRestore(); removed.mockRestore();
});

test("applied search field stays synchronized across column sort and pager reload even if select changes", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const searchQueries: unknown[] = [];
  const multiFieldDescription = {
    ...description,
    fields: {
      "test.note": {
        name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null },
        description: { type: "text", label: "Description", required: false, readonly: false, relation: null, relation_field: null, selection: null }
      }
    },
    views: [
      {
        identity: { kind: "stored", id: 2 },
        type: "list",
        schema_major: 1,
        renderer: "web.list",
        root: 0,
        nodes: [
          { kind: "list", children: [1, 2] },
          { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" },
          { kind: "field", force_save: false, field: { model: "test.note", name: "description" }, widget: "text", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Description" }
        ]
      }
    ]
  };

  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const body = init?.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      searchQueries.push(body.domain);
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, multiFieldDescription) : path.endsWith("get_views") ? multiFieldDescription
      : path.endsWith("web_search_read") ? { length: 50, records: [{ id: 1, name: "Alpha Note", description: "First" }] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const searchInput = root.querySelector<HTMLInputElement>(".search-input")!;
  const searchForm = root.querySelector<HTMLFormElement>(".search-form")!;
  const fieldSelect = root.querySelector<HTMLSelectElement>(".search-field-select")!;

  expect(fieldSelect.value).toBe("name");
  searchInput.value = "Alpha";
  searchForm.dispatchEvent(new Event("submit"));

  await vi.waitFor(() => expect(searchQueries.some(q => JSON.stringify(q) === JSON.stringify([["name", "ilike", "Alpha"]]))).toBe(true));

  // User changes select to 'description' in dropdown without clicking Search
  fieldSelect.value = "description";

  // Now user clicks Next on pager
  const nextBtn = root.querySelector<HTMLButtonElement>(".pager-next")!;
  nextBtn.click();

  // The request domain must still use 'name', NOT 'description'
  await vi.waitFor(() => expect(searchQueries.length).toBeGreaterThan(2));
  expect(searchQueries[searchQueries.length - 1]).toEqual([["name", "ilike", "Alpha"]]);

  // Now user clicks column header to sort
  const sortBtn = root.querySelector<HTMLButtonElement>("table th button")!;
  sortBtn.click();

  await vi.waitFor(() => expect(searchQueries.length).toBeGreaterThan(3));
  expect(searchQueries[searchQueries.length - 1]).toEqual([["name", "ilike", "Alpha"]]);
});

test("secondary navbar dropdown closes on outside click and focusout", async () => {
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11], icon: null },
        { module: null, id: 11, parent_id: 10, label: "Folders", action_id: null, children: [12], icon: null },
        { module: null, id: 12, parent_id: 11, label: "Archive", action_id: 20, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const navDropdown = root.querySelector<HTMLElement>(".nav-dropdown")!;
  const toggleBtn = navDropdown.querySelector<HTMLButtonElement>(".nav-dropdown-toggle")!;
  const menuList = navDropdown.querySelector<HTMLElement>(".nav-dropdown-menu")!;

  expect(menuList.classList.contains("open")).toBe(false);

  // Open dropdown
  toggleBtn.click();
  expect(menuList.classList.contains("open")).toBe(true);

  // Click outside (on document body)
  document.body.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  expect(menuList.classList.contains("open")).toBe(false);

  // Open dropdown again
  toggleBtn.click();
  expect(menuList.classList.contains("open")).toBe(true);

  // Focus may move between controls inside the dropdown without closing it.
  const item = menuList.querySelector<HTMLButtonElement>("button")!;
  navDropdown.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: item }));
  expect(menuList.classList.contains("open")).toBe(true);

  // A null relatedTarget can occur during a mouse click. Keep the menu open
  // while focus remains on a control inside it, so the click can land.
  const itemClick = vi.fn();
  item.addEventListener("click", event => {
    itemClick();
    event.stopImmediatePropagation();
  }, { capture: true });
  toggleBtn.focus();
  navDropdown.dispatchEvent(new FocusEvent("focusout", { bubbles: true }));
  expect(menuList.classList.contains("open")).toBe(true);
  item.click();
  expect(itemClick).toHaveBeenCalledOnce();
  expect(menuList.classList.contains("open")).toBe(true);
  await new Promise(resolve => window.setTimeout(resolve, 0));
  expect(menuList.classList.contains("open")).toBe(true);

  // When focus actually leaves and the browser reports no related target,
  // the deferred check still dismisses the menu.
  toggleBtn.blur();
  navDropdown.dispatchEvent(new FocusEvent("focusout", { bubbles: true }));
  expect(menuList.classList.contains("open")).toBe(true);
  await new Promise(resolve => window.setTimeout(resolve, 0));
  expect(menuList.classList.contains("open")).toBe(false);

  // Focusout with relatedTarget outside closes the menu.
  toggleBtn.click();
  expect(menuList.classList.contains("open")).toBe(true);
  navDropdown.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: document.body }));
  expect(menuList.classList.contains("open")).toBe(false);
});

test("Escape key closes home menu overlay, mobile drawer, user dropdown, and nav dropdowns", async () => {
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11], icon: null },
        { module: null, id: 11, parent_id: 10, label: "Folders", action_id: null, children: [12], icon: null },
        { module: null, id: 12, parent_id: 11, label: "Archive", action_id: 20, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const documentAdd = vi.spyOn(document, "addEventListener");
  const windowAdd = vi.spyOn(window, "addEventListener");
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const docClickListener = documentAdd.mock.calls.find(([type]) => type === "click")?.[1];
  const windowKeyListener = windowAdd.mock.calls.find(([type]) => type === "keydown")?.[1];
  expect(docClickListener).toBeTypeOf("function");
  expect(windowKeyListener).toBeTypeOf("function");

  // Test 1: Home Menu overlay closed by Escape
  const appSwitcher = root.querySelector<HTMLButtonElement>("[data-app-switcher]")!;
  const homeOverlay = root.querySelector<HTMLElement>(".home-menu-overlay")!;
  appSwitcher.click();
  expect(homeOverlay.classList.contains("hidden")).toBe(false);
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  expect(homeOverlay.classList.contains("hidden")).toBe(true);

  // Test 2: Mobile drawer closed by Escape
  const burgerBtn = root.querySelector<HTMLButtonElement>("[data-burger-menu]")!;
  const mobileDrawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  burgerBtn.click();
  expect(mobileDrawer.classList.contains("hidden")).toBe(false);
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  expect(mobileDrawer.classList.contains("hidden")).toBe(true);

  // Test 3: User dropdown closed by Escape
  const userBtn = root.querySelector<HTMLButtonElement>(".user-button")!;
  const userDropdown = root.querySelector<HTMLElement>(".user-dropdown")!;
  userBtn.click();
  expect(userDropdown.classList.contains("open")).toBe(true);
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  expect(userDropdown.classList.contains("open")).toBe(false);

  // Test 4: Nav dropdown closed by Escape
  const navToggle = root.querySelector<HTMLButtonElement>(".nav-dropdown-toggle")!;
  const navMenu = root.querySelector<HTMLElement>(".nav-dropdown-menu")!;
  navToggle.click();
  expect(navMenu.classList.contains("open")).toBe(true);
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  expect(navMenu.classList.contains("open")).toBe(false);

  const documentRemove = vi.spyOn(document, "removeEventListener");
  const windowRemove = vi.spyOn(window, "removeEventListener");
  app.dispose();
  expect(documentRemove).toHaveBeenCalledWith("click", docClickListener);
  expect(windowRemove).toHaveBeenCalledWith("keydown", windowKeyListener);
});

test("root /erp navigation reuses buildHomeContent with app search input and tiles", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10, 11],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
        { module: null, id: 11, parent_id: null, label: "Settings", action_id: 21, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const content = root.querySelector("[data-client-content]")!;
  const searchInput = content.querySelector<HTMLInputElement>(".home-menu-search")!;
  expect(searchInput).not.toBeNull();

  const notesTile = content.querySelector<HTMLElement>('.app-tile[data-menu-id="10"]')!;
  const settingsTile = content.querySelector<HTMLElement>('.app-tile[data-menu-id="11"]')!;
  expect(notesTile).not.toBeNull();
  expect(settingsTile).not.toBeNull();

  // The primary homepage must not also exist in an open navigation overlay.
  expect(root.querySelectorAll('.app-tile[data-menu-id="10"]')).toHaveLength(1);
  root.querySelector<HTMLButtonElement>('[data-app-switcher]')!.click();
  expect(document.activeElement).toBe(searchInput);
  expect(root.querySelectorAll('.app-tile[data-menu-id="10"]')).toHaveLength(1);

  // Search filters tiles in /erp content
  searchInput.value = "Settings";
  searchInput.dispatchEvent(new Event("input"));
  expect(settingsTile.style.display).toBe("");
  expect(notesTile.style.display).toBe("none");

  // Clicking an app tile navigates to the app
  searchInput.value = "";
  searchInput.dispatchEvent(new Event("input"));
  notesTile.click();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/erp/action-20"));
});

test("search field select defaults to name or display_name when available", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const multiFieldDescription = {
    ...description,
    fields: {
      "test.note": {
        code: { type: "char", label: "Reference Code", required: false, readonly: false, relation: null, relation_field: null, selection: null },
        name: { type: "char", label: "Subject", required: true, readonly: false, relation: null, relation_field: null, selection: null },
        notes: { type: "text", label: "Notes", required: false, readonly: false, relation: null, relation_field: null, selection: null }
      }
    },
    views: [
      {
        identity: { kind: "stored", id: 2 },
        type: "list",
        schema_major: 1,
        renderer: "web.list",
        root: 0,
        nodes: [
          { kind: "list", children: [1, 2, 3] },
          { kind: "field", force_save: false, field: { model: "test.note", name: "code" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Reference Code" },
          { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Subject" },
          { kind: "field", force_save: false, field: { model: "test.note", name: "notes" }, widget: "text", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Notes" }
        ]
      }
    ]
  };

  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 2, type: "list" }],
      domain: null,
      context: null
    }, multiFieldDescription) : path.endsWith("get_views") ? multiFieldDescription
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const fieldSelect = root.querySelector<HTMLSelectElement>(".search-field-select")!;
  // Even though 'code' is the first field in the list, 'name' should be selected by default
  expect(fieldSelect.value).toBe("name");
});

test("cancelled company switch resets company select to currentCompanyId", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });

  const multiCompanyBootstrap = {
    ...bootstrap,
    companies: [{ id: 1, name: "Company 1" }, { id: 2, name: "Company 2" }],
    current_company_id: 2,
    allowed_company_ids: [1, 2],
    active_company_ids: [1, 2]
  };

  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const body = init?.body ? JSON.parse(String(init.body)) : {};
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1,
      registry_version: "r1",
      roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1,
      id: 20,
      name: "Notes",
      model: "test.note",
      target: "current",
      views: [{ id: 1, type: "form" }],
      domain: null,
      context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_read") ? [{ id: 5, name: "Initial note" }]
      : path.endsWith("onchange") ? { value: { name: objectName(body["values"]) } }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, multiCompanyBootstrap);
  await app.start();

  const companySelect = root.querySelector<HTMLSelectElement>('.user-dropdown-company select[aria-label="Current company"]')!;
  expect(companySelect.value).toBe("2");

  // Dirty the form
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Unsaved draft";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));

  // Try switching to Company 1
  companySelect.value = "1";
  companySelect.dispatchEvent(new Event("change"));

  // Dialog pops up: user clicks "Cancel"
  await vi.waitFor(() => expect(document.querySelectorAll("dialog")).toHaveLength(1));
  const cancelBtn = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(btn => btn.textContent === "Cancel")!;
  cancelBtn.click();

  // Verification: companySelect value must be reset to "2" (currentCompanyId)
  await vi.waitFor(() => expect(companySelect.value).toBe("2"));
});

test.each([
  ["absent from bootstrap companies", { current_company_id: 99 }],
  ["outside allowed companies", {
    companies: [{ id: 1, name: "Company 1" }, { id: 2, name: "Company 2" }],
    current_company_id: 2
  }],
  ["outside active companies", {
    companies: [{ id: 1, name: "Company 1" }, { id: 2, name: "Company 2" }],
    current_company_id: 2,
    allowed_company_ids: [1, 2]
  }]
])("rejects a current company %s", (_case, companyContext) => {
  const root = host();
  const invalidBootstrap = {
    ...bootstrap,
    ...companyContext
  };

  expect(() => new HumanApp(root, invalidBootstrap)).toThrow(expect.objectContaining({
    kind: "server",
    message: "Invalid company context."
  }));
});

test("form List switcher clears record id and leaves a reloadable search URL", async()=>{
 history.replaceState(null,"","/erp/action-20?id=5&view_type=form&search=Meeting");
 vi.stubGlobal("fetch",vi.fn(async(path:string)=>{
  const value=path.endsWith("erp_load_web_menus")?{wire_major:1,registry_version:"r1",roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:"Notes",action_id:20,children:[],icon:null}]}:
   (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views"))?actionMetadata(path, {wire_major:1,id:20,name:"Notes",model:"test.note",target:"current",views:[{id:1,type:"form"},{id:2,type:"list"}],domain:null,context:null}, description):
   path.endsWith("get_views")?description:path.endsWith("web_search_read")?{length:1,records:[{id:5,name:"Meeting"}]}:[{id:5,name:"Meeting"}];
  return new Response(JSON.stringify(value),{headers:{"Content-Type":"application/json"}});
 }));
 const root=host();app=new HumanApp(root,bootstrap);await app.start();
 root.querySelector<HTMLButtonElement>('[aria-label="list view"]')!.click();
 await vi.waitFor(()=>expect(root.querySelector("table")).not.toBeNull());
 expect(new URLSearchParams(location.search).has("id")).toBe(false);
 expect(new URLSearchParams(location.search).get("view_type")).toBe("list");
 const locationBefore=location.href;app.dispose();app=new HumanApp(root,bootstrap);await app.start();
 expect(root.querySelector("table")).not.toBeNull();expect(location.href).toBe(locationBefore);
});

test("nested menu categories render dropdown headers and submenus correctly", async () => {
  history.replaceState(null, "", "/erp/action-100?menu=10");
  const menuData = {
    wire_major: 1,
    registry_version: "r1",
    roots: [1],
    nodes: [
      { module: null, id: 1, parent_id: null, label: "Settings", action_id: null, children: [2], icon: null },
      { module: null, id: 2, parent_id: 1, label: "Technical", action_id: null, children: [3, 4], icon: null },
      { module: null, id: 3, parent_id: 2, label: "Database Structure", action_id: null, children: [10], icon: null },
      { module: null, id: 4, parent_id: 2, label: "User Interface", action_id: null, children: [11], icon: null },
      { module: null, id: 10, parent_id: 3, label: "Models", action_id: 100, children: [], icon: null },
      { module: null, id: 11, parent_id: 4, label: "Views", action_id: 101, children: [], icon: null }
    ]
  };

  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? menuData
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
        wire_major: 1,
        id: 100,
        name: "Models",
        model: "test.note",
        target: "current",
        views: [{ id: 2, type: "list" }],
        domain: null,
        context: null
      }, description)
      : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const nav = root.querySelector("[data-client-navigation]")!;
  const toggle = nav.querySelector<HTMLButtonElement>(".nav-dropdown-toggle")!;
  expect(toggle.textContent).toContain("Technical");

  const dropdownMenu = nav.querySelector<HTMLElement>(".nav-dropdown-menu")!;
  expect(dropdownMenu.classList.contains("open")).toBe(false);

  // Click toggle opens dropdown
  toggle.click();
  expect(dropdownMenu.classList.contains("open")).toBe(true);

  // Verify dropdown headers
  const headers = dropdownMenu.querySelectorAll(".dropdown-header");
  expect(headers.length).toBe(2);
  expect(headers[0]?.textContent).toBe("Database Structure");
  expect(headers[1]?.textContent).toBe("User Interface");

  // Verify dropdown items
  const items = dropdownMenu.querySelectorAll<HTMLButtonElement>(".dropdown-item");
  expect(items.length).toBe(2);
  expect(items[0]?.textContent).toBe("Models");
  expect(items[0]?.dataset["menuId"]).toBe("10");
  expect(items[1]?.textContent).toBe("Views");
  expect(items[1]?.dataset["menuId"]).toBe("11");

  // Verify mobile drawer renders subtitles and nested items
  const drawerSubtitles = root.querySelectorAll(".drawer-section-subtitle");
  expect(drawerSubtitles.length).toBeGreaterThanOrEqual(2);
  const drawerItems = root.querySelectorAll<HTMLButtonElement>(".mobile-drawer .drawer-item");
  const itemLabels = Array.from(drawerItems).map(i => i.textContent);
  expect(itemLabels).toContain("Models");
  expect(itemLabels).toContain("Views");
});

function stubMenus(nodes: unknown[], roots: number[]): void {
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots, nodes }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
        wire_major: 1, id: 100, name: "Models", model: "test.note", target: "current",
        views: [{ id: 2, type: "list" }], domain: null, context: null
      }, description)
      : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
}


test("categories without a reachable action are omitted instead of rendering empty dropdowns", async () => {
  history.replaceState(null, "", "/erp/action-100?menu=10");
  stubMenus([
    { module: null, id: 1, parent_id: null, label: "Settings", action_id: null, children: [2, 5], icon: null },
    { module: null, id: 2, parent_id: 1, label: "Technical", action_id: null, children: [3, 4, 6], icon: null },
    { module: null, id: 3, parent_id: 2, label: "Database Structure", action_id: null, children: [10], icon: null },
    { module: null, id: 4, parent_id: 2, label: "Empty Category", action_id: null, children: [], icon: null },
    { module: null, id: 6, parent_id: 2, label: "Nested Empty", action_id: null, children: [7], icon: null },
    { module: null, id: 7, parent_id: 6, label: "Inner Empty", action_id: null, children: [], icon: null },
    { module: null, id: 5, parent_id: 1, label: "Ghost", action_id: null, children: [8], icon: null },
    { module: null, id: 8, parent_id: 5, label: "Ghost Category", action_id: null, children: [], icon: null },
    { module: null, id: 10, parent_id: 3, label: "Models", action_id: 100, children: [], icon: null }
  ], [1]);
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const nav = root.querySelector("[data-client-navigation]")!;
  const toggles = [...nav.querySelectorAll<HTMLButtonElement>(".nav-dropdown-toggle")].map(toggle => toggle.textContent);
  expect(toggles).toEqual(["Technical"]);
  const headers = [...nav.querySelectorAll(".nav-dropdown-menu .dropdown-header")].map(header => header.textContent);
  expect(headers).toEqual(["Database Structure"]);
  const subtitles = [...root.querySelectorAll(".mobile-drawer .drawer-section-subtitle")].map(subtitle => subtitle.textContent);
  expect(subtitles).toEqual(["Technical", "Database Structure"]);
});


test("opening one navigation dropdown closes the other, and an outside click closes it", async () => {
  history.replaceState(null, "", "/erp/action-100?menu=10");
  stubMenus([
    { module: null, id: 1, parent_id: null, label: "Settings", action_id: null, children: [2, 3], icon: null },
    { module: null, id: 2, parent_id: 1, label: "Technical", action_id: null, children: [10], icon: null },
    { module: null, id: 3, parent_id: 1, label: "Users", action_id: null, children: [11], icon: null },
    { module: null, id: 10, parent_id: 2, label: "Models", action_id: 100, children: [], icon: null },
    { module: null, id: 11, parent_id: 3, label: "Groups", action_id: 101, children: [], icon: null }
  ], [1]);
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const nav = root.querySelector("[data-client-navigation]")!;
  const [technical, users] = [...nav.querySelectorAll<HTMLButtonElement>(".nav-dropdown-toggle")];
  const [technicalMenu, usersMenu] = [...nav.querySelectorAll<HTMLElement>(".nav-dropdown-menu")];
  technical!.click();
  expect(technicalMenu!.classList.contains("open")).toBe(true);
  expect(usersMenu!.classList.contains("open")).toBe(false);
  users!.click();
  expect(technicalMenu!.classList.contains("open")).toBe(false);
  expect(usersMenu!.classList.contains("open")).toBe(true);
  users!.click();
  expect(usersMenu!.classList.contains("open")).toBe(false);

  technical!.click();
  expect(technicalMenu!.classList.contains("open")).toBe(true);
  // A click inside the open dropdown keeps it open; a click elsewhere in the page closes it.
  technicalMenu!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  expect(technicalMenu!.classList.contains("open")).toBe(true);
  root.querySelector<HTMLElement>("[data-client-content]")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  expect(technicalMenu!.classList.contains("open")).toBe(false);
});


test("a cyclic menu tree is refused before any navigation renders", async () => {
  history.replaceState(null, "", "/erp/action-100?menu=10");
  stubMenus([
    { module: null, id: 1, parent_id: null, label: "Settings", action_id: null, children: [2], icon: null },
    { module: null, id: 2, parent_id: 1, label: "Technical", action_id: null, children: [3], icon: null },
    { module: null, id: 3, parent_id: 2, label: "Loop", action_id: null, children: [2], icon: null }
  ], [1]);
  const root = host();
  app = new HumanApp(root, bootstrap);
  await expect(app.start()).rejects.toThrow("Invalid menu tree.");
  expect(root.querySelector(".nav-dropdown")).toBeNull();
});

test("failed initial form render disposes partial widgets and action listeners before retry",async()=>{
 history.replaceState(null,"","/erp/action-20?id=5&view_type=form");
 const added=vi.spyOn(document,"addEventListener"),removed=vi.spyOn(document,"removeEventListener");
 const disposed=vi.fn();let renders=0,fail=true;
 registerFieldWidget("test.form_failure_cleanup",{kinds:["char"],render:()=>{
  if(++renders===2 && fail)throw new Error("Fixture widget render failed");
  return {node:document.createElement("input"),dispose:disposed};
 }});
 const broken=structuredClone(description);
 Object.assign(broken.views[0]!.nodes[1]!,{widget:"test.form_failure_cleanup"});
 broken.views[0]!.nodes.push({kind:"field", force_save: false,field:{model:"test.note",name:"name"},widget:"test.form_failure_cleanup",options:{},modifiers:{readonly:null,required:null,invisible:null},label:"Name"});
 broken.views[0]!.nodes[0]!.children=[1,2];
 vi.stubGlobal("fetch",vi.fn(async(path:string)=>{
  const value=path.endsWith("erp_load_web_menus")?{wire_major:1,registry_version:"r1",roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:"Notes",action_id:20,children:[],icon:null}]}
   :(path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views"))?actionMetadata(path, {wire_major:1,id:20,name:"Notes",model:"test.note",target:"current",views:[{id:1,type:"form"}],domain:null,context:null}, broken)
   :path.endsWith("get_views")?broken:[{id:5,name:"Draft"}];
  return new Response(JSON.stringify(value),{headers:{"Content-Type":"application/json"}});
 }));
 const root=host();app=new HumanApp(root,bootstrap);
 added.mockClear();removed.mockClear();
 await expect(app.start()).rejects.toThrow("Fixture widget render failed");
 expect(disposed).toHaveBeenCalledTimes(1);
 const outstanding=added.mock.calls.filter(([type,callback])=>type==="click"&&!removed.mock.calls.some(([removedType,removedCallback])=>type===removedType&&callback===removedCallback));
 expect(outstanding).toHaveLength(0);
 fail=false;await app.start();
 expect(root.querySelectorAll(".record-form input")).toHaveLength(2);
 app.dispose();app=undefined;
 expect(disposed).toHaveBeenCalledTimes(3);
});


test("invalid Lucide menu identity cannot fall through to external image rendering and shows the default icon", async () => {
  history.replaceState(null, "", "/erp");
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({
    wire_major: 1, registry_version: "r1", roots: [10, 11], nodes: [
      { module: null,id:10,parent_id:null,label:"Notes",action_id:20,children:[],icon:"lucide:https://evil.example/icon.svg"},
      { module: null,id:11,parent_id:null,label:"Library",action_id:21,children:[],icon:"lucide:library"}
    ]
  }), {headers:{"Content-Type":"application/json"}})));
  const root = host(); app = new HumanApp(root, bootstrap);
  await app.start();
  expect([...root.querySelectorAll("img")].filter(img => !["/erp/brand/logo.svg", "/erp/brand/logo-dark.svg"].includes(img.getAttribute("src") ?? ""))).toEqual([]);
  expect(root.querySelector('[data-menu-id="10"] .app-icon svg')?.getAttribute("data-icon")).toBe("layout-grid");
  expect(root.querySelector('[data-menu-id="11"] .app-icon svg')?.getAttribute("data-icon")).toBe("library");
  expect(root.innerHTML).not.toContain("evil.example");
  expect(warn).toHaveBeenCalledWith(expect.stringContaining("is not a shipped icon"));
  warn.mockRestore();
});

async function mobileNavigationFixture(): Promise<HTMLElement> {
  history.replaceState(null, "", "/erp/action-20?menu=11");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10, 30], nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11], icon: null },
        { module: null, id: 11, parent_id: 10, label: "Archive", action_id: 20, children: [], icon: null },
        { module: null, id: 30, parent_id: null, label: "Other", action_id: 40, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }], domain: null, context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  root.querySelector<HTMLButtonElement>("[data-burger-menu]")!.click();
  return root;
}

function drawerPointer(target: EventTarget, type: string, x: number, y: number, id = 1, pointerType = "touch"): Event {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperties(event, {
    pointerId: { value: id }, pointerType: { value: pointerType },
    clientX: { value: x }, clientY: { value: y }, button: { value: 0 }
  });
  target.dispatchEvent(event);
  return event;
}

test("mobile drawer exposes modal focus, toggle state and backdrop dismissal with restoration", async () => {
  document.body.style.overflow = "auto";
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  const toggle = root.querySelector<HTMLButtonElement>("[data-burger-menu]")!;
  const close = drawer.querySelector<HTMLButtonElement>(".drawer-close-btn")!;
  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  expect(toggle.getAttribute("aria-controls")).toBe(drawer.id);
  expect(drawer.getAttribute("role")).toBe("dialog");
  expect(drawer.getAttribute("aria-modal")).toBe("true");
  expect(document.activeElement).toBe(close);
  expect(document.body.style.overflow).toBe("hidden");
  expect(root.querySelector<HTMLElement>("[data-client-content]")!.inert).toBe(true);
  expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(true);
  close.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(drawer.querySelectorAll("button")[drawer.querySelectorAll("button").length - 1]);
  document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(close);
  root.querySelector<HTMLElement>(".mobile-drawer-backdrop")!.click();
  expect(drawer.classList.contains("hidden")).toBe(true);
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(toggle);
  expect(document.body.style.overflow).toBe("auto");
  expect(root.querySelector<HTMLElement>("[data-client-content]")!.inert).toBe(false);
  expect(document.documentElement.classList.contains("erp-scroll-locked")).toBe(false);
  document.body.style.overflow = "";
});

test("mobile drawer left swipe tracks drag, closes, and suppresses accidental menu activation", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  const item = drawer.querySelector<HTMLButtonElement>(".drawer-item")!;
  const callsBefore = vi.mocked(fetch).mock.calls.length;
  drawerPointer(item, "pointerdown", 220, 100);
  drawerPointer(item, "pointermove", 110, 104);
  expect(drawer.style.transform).toBe("translateX(-110px)");
  drawerPointer(item, "pointerup", 110, 104);
  expect(drawer.classList.contains("hidden")).toBe(true);
  item.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, detail: 1 }));
  expect(vi.mocked(fetch).mock.calls.length).toBe(callsBefore);
  expect(root.querySelector("[data-burger-menu]")!.getAttribute("aria-expanded")).toBe("false");
});

test("mobile drawer short drags, vertical and rightward intent, cancellation and multitouch stay open", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  const sequences = [
    [[200, 100], [180, 101]],
    [[200, 100], [198, 120], [70, 125]],
    [[100, 100], [130, 100], [0, 100]],
  ];
  for (const points of sequences) {
    drawerPointer(drawer, "pointerdown", points[0]![0]!, points[0]![1]!);
    for (const point of points.slice(1)) drawerPointer(drawer, "pointermove", point[0]!, point[1]!);
    const last = points[points.length - 1]!;
    drawerPointer(drawer, "pointerup", last[0]!, last[1]!);
    expect(drawer.classList.contains("hidden")).toBe(false);
    expect(drawer.style.transform).toBe("");
  }
  drawerPointer(drawer, "pointerdown", 220, 100);
  drawerPointer(drawer, "pointermove", 80, 100);
  drawerPointer(drawer, "pointercancel", 80, 100);
  expect(drawer.classList.contains("hidden")).toBe(false);
  drawerPointer(drawer, "pointerdown", 220, 100);
  drawerPointer(drawer, "pointerdown", 210, 100, 2);
  drawerPointer(drawer, "pointermove", 60, 100);
  drawerPointer(drawer, "pointerup", 60, 100);
  drawerPointer(drawer, "pointerup", 50, 100, 2);
  expect(drawer.classList.contains("hidden")).toBe(false);
  expect(drawer.style.transform).toBe("");
});

test("mobile drawer ignores mouse gestures and disposes open modal side effects and listeners", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  drawerPointer(drawer, "pointerdown", 200, 100, 1, "mouse");
  drawerPointer(drawer, "pointermove", 20, 100, 1, "mouse");
  drawerPointer(drawer, "pointerup", 20, 100, 1, "mouse");
  expect(drawer.classList.contains("hidden")).toBe(false);
  const remove = vi.spyOn(window, "removeEventListener");
  app!.dispose();
  expect(document.body.style.overflow).toBe("");
  expect(root.querySelector<HTMLElement>("[data-client-content]")!.inert).toBe(false);
  expect(remove.mock.calls.some(([name]) => name === "pointerup")).toBe(true);
  expect(remove.mock.calls.some(([name]) => name === "resize")).toBe(true);
  root.querySelector<HTMLButtonElement>("[data-burger-menu]")!.click();
  expect(drawer.classList.contains("hidden")).toBe(true);
});

test("mobile drawer desktop resize restores scroll and focuses the visible navigation", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  vi.stubGlobal("innerWidth", 1100);
  window.dispatchEvent(new Event("resize"));
  expect(drawer.classList.contains("hidden")).toBe(true);
  expect(document.body.style.overflow).toBe("");
  expect(document.activeElement).toBe(root.querySelector("[data-app-switcher]"));
});

test("Office Blue desktop resize restores focus to visible sidebar navigation", async () => {
  const root = await mobileNavigationFixture();
  document.documentElement.dataset["officeTheme"] = "office-blue";
  try {
    const selected = root.querySelector<HTMLElement>(".office-navigation [aria-current=page]")!;
    const ancestry = selected.closest("details")!;
    ancestry.open = false;
    vi.stubGlobal("innerWidth", 1100);
    window.dispatchEvent(new Event("resize"));
    expect(ancestry.open).toBe(true);
    expect(root.querySelector(".mobile-drawer")?.classList.contains("hidden")).toBe(true);
    expect(document.body.style.overflow).toBe("");
    const current = root.querySelector(".office-navigation [aria-current=page]");
    expect(document.activeElement).toBe(current ?? root.querySelector("button.brand"));
    expect(document.activeElement).not.toBe(root.querySelector("[data-app-switcher]"));
  } finally { document.documentElement.removeAttribute("data-office-theme"); }
});

test("mobile drawer history rerender releases the old modal and replacement opens once", async () => {
  const root = await mobileNavigationFixture();
  const oldDrawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  const remove = vi.spyOn(window, "removeEventListener");
  history.replaceState({ erpIndex: -1 }, "", "/erp");
  window.dispatchEvent(new PopStateEvent("popstate", { state: { erpIndex: -1 } }));
  await vi.waitFor(() => expect(oldDrawer.isConnected).toBe(false));
  expect(document.body.style.overflow).toBe("");
  expect(root.querySelector<HTMLElement>("[data-client-content]")!.inert).toBe(false);
  expect(remove.mock.calls.filter(([name]) => name === "pointerup")).toHaveLength(1);
  const replacement = root.querySelector<HTMLElement>(".mobile-drawer")!;
  root.querySelector<HTMLButtonElement>("[data-burger-menu]")!.click();
  expect(replacement.classList.contains("hidden")).toBe(false);
  expect(document.body.style.overflow).toBe("hidden");
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
  expect(replacement.classList.contains("hidden")).toBe(true);
  expect(document.body.style.overflow).toBe("");
});

test("mobile drawer canceled drag preserves subsequent close-button keyboard activation", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  drawerPointer(drawer, "pointerdown", 220, 100);
  drawerPointer(drawer, "pointermove", 80, 100);
  drawerPointer(drawer, "pointercancel", 80, 100);
  expect(drawer.classList.contains("hidden")).toBe(false);
  // Keyboard activation dispatches a click without starting a new pointer sequence.
  drawer.querySelector<HTMLButtonElement>(".drawer-close-btn")!.click();
  expect(drawer.classList.contains("hidden")).toBe(true);
  expect(document.body.style.overflow).toBe("");
});

test("mobile drawer ignores bubbled descendant capture loss when taking over a touch drag", async () => {
  const root = await mobileNavigationFixture();
  const drawer = root.querySelector<HTMLElement>(".mobile-drawer")!;
  const close = drawer.querySelector<HTMLButtonElement>(".drawer-close-btn")!;
  drawerPointer(close, "pointerdown", 225, 30);
  drawerPointer(close, "pointermove", 200, 31);
  // Native touch starts implicitly captured by the button. Capture transfer to the
  // panel emits loss on that descendant; it is not loss of the panel's own capture.
  drawerPointer(close, "lostpointercapture", 200, 31);
  drawerPointer(drawer, "pointermove", 80, 32);
  expect(drawer.style.transform).toBe("translateX(-145px)");
  drawerPointer(drawer, "pointerup", 80, 32);
  expect(drawer.classList.contains("hidden")).toBe(true);
});


test("operation failures use an accessible dialog, preserve content and restore focus", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  const root = host(), trigger = document.createElement("button"); trigger.textContent = "Open record"; root.append(trigger); trigger.focus();
  app = new HumanApp(root, bootstrap);
  app.run(async () => { throw new Error("Renderer failed <img src=x>"); });
  await vi.waitFor(() => expect(document.querySelector("dialog.client-error-dialog")?.textContent).toContain("Renderer failed <img src=x>"));
  const dialog = document.querySelector<HTMLDialogElement>("dialog.client-error-dialog")!;
  expect(dialog.getAttribute("aria-labelledby")).toBeTruthy();
  expect(dialog.querySelector("img")).toBeNull();
  expect(dialog.querySelector("details")?.open).toBe(false);
  expect(root.contains(trigger)).toBe(true);
  dialog.querySelector<HTMLButtonElement>('[data-error-close]')!.click();
  expect(dialog.isConnected).toBe(false);
  expect(document.activeElement).toBe(trigger);
});

function metadataServer(model = "test.note", bundle?: () => Promise<void>) {
  const calls: string[] = [];
  const modelDescription = JSON.parse(JSON.stringify(description).replaceAll('"test.note"', JSON.stringify(model))) as typeof description;
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    calls.push(path.split("/").slice(-1)[0]!);
    if (path.endsWith("erp_load_web_action_views") && bundle) await bundle();
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11, 12], icon: null },
        { module: null, id: 11, parent_id: 10, label: "All notes", action_id: 20, children: [], icon: null },
        { module: null, id: 12, parent_id: 10, label: "Archive", action_id: 21, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model, target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
    }, modelDescription) : path.endsWith("get_views") ? modelDescription
      : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Note A" }] }
      : path.endsWith("web_read") ? [{ id: 5, name: "Note A" }]
      : path.endsWith("onchange") ? { value: { name: "Note B" } }
      : path.endsWith("web_save") ? [{ id: 5, name: "Note B" }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  return calls;
}

const count = (calls: string[], method: string): number => calls.filter(call => call === method).length;

test("opening a record and returning to its list reuse the action and view metadata", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=11");
  const calls = metadataServer();
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  root.querySelector<HTMLElement>("tbody tr")!.click();
  await vi.waitFor(() => expect(root.querySelector(".record-form")).not.toBeNull());
  root.querySelector<HTMLButtonElement>('[data-menu-id="11"]')!.click();
  await vi.waitFor(() => expect(count(calls, "web_search_read")).toBe(2));
  await vi.waitFor(() => expect(root.querySelector("tbody tr")).not.toBeNull());
  expect(count(calls, "erp_load_web_action_views")).toBe(1);
  expect(count(calls, "get_views")).toBe(0);
  expect(count(calls, "web_read")).toBe(1);
});

test("resting on a menu entry loads its metadata in the background, so the click waits only for data", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    history.replaceState(null, "", "/erp");
    const calls = metadataServer();
    const root = host();
    app = new HumanApp(root, bootstrap);
    await app.start();
    expect(count(calls, "erp_load_web_action_views")).toBe(0);
    const tile = root.querySelector<HTMLElement>('.app-tile[data-menu-id="10"]')!;
    tile.dispatchEvent(new Event("pointerover", { bubbles: true }));
    tile.dispatchEvent(new Event("pointerout", { bubbles: true }));
    await vi.advanceTimersByTimeAsync(200);
    expect(count(calls, "erp_load_web_action_views")).toBe(0);
    tile.dispatchEvent(new Event("pointerover", { bubbles: true }));
    await vi.advanceTimersByTimeAsync(200);
    await vi.waitFor(() => expect(count(calls, "erp_load_web_action_views")).toBe(1));
    expect(root.querySelector(".client-loading")?.hasAttribute("hidden") ?? true).toBe(true);
    tile.click();
    await vi.waitFor(() => expect(root.querySelector("tbody tr")).not.toBeNull());
    expect(count(calls, "erp_load_web_action_views")).toBe(1);
    expect(count(calls, "get_views")).toBe(0);
    expect(count(calls, "web_search_read")).toBe(1);
  } finally {
    vi.useRealTimers();
  }
});

test("saving a technical record drops the cached metadata, so the next navigation reloads it", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=11&id=5&view_type=form");
  const calls = metadataServer("ir.ui.view");
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Note B";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(count(calls, "web_save")).toBe(1));
  await vi.waitFor(() => expect(root.textContent).toContain("Record saved."));
  root.querySelector<HTMLButtonElement>('[data-menu-id="11"]')!.click();
  await vi.waitFor(() => expect(count(calls, "web_search_read")).toBe(1));
  expect(count(calls, "erp_load_web_action_views")).toBe(2);
  expect(count(calls, "get_views")).toBe(0);
});

test("menu prefetches run one at a time and skip entries the pointer already left behind", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    history.replaceState(null, "", "/erp/action-20?menu=11");
    const actions: number[] = [];
    let inFlight = 0, peak = 0;
    // The first prefetch stays in flight until every hover is done. A timed delay raced real time under
    // shouldAdvanceTime: on a loaded host it resolved while the pointer was still on menu 14, so 23 ran.
    let release!: () => void;
    const held = new Promise<void>(resolve => { release = resolve; });
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as { ids?: number[] };
      inFlight++; peak = Math.max(peak, inFlight);
      try {
        if (path.endsWith("erp_load_web_action_views")) {
          actions.push(body.ids![0]!);
          if (actions.length > 1) await held;
        }
        const value = path.endsWith("erp_load_web_menus") ? {
          wire_major: 1, registry_version: "r1", roots: [10],
          nodes: [
            { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11, 12, 13, 14, 15], icon: null },
            ...[11, 12, 13, 14, 15].map(id => ({ module: null, id, parent_id: 10, label: `Menu ${id}`, action_id: id + 9, children: [], icon: null }))
          ]
        } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
          wire_major: 1, id: body.ids![0], name: "Notes", model: "test.note", target: "current",
          views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
        }, description) : path.endsWith("get_views") ? description
          : path.endsWith("web_search_read") ? { length: 1, records: [{ id: 5, name: "Note A" }] }
          : [];
        return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
      } finally { inFlight--; }
    }));
    const root = host();
    app = new HumanApp(root, bootstrap);
    await app.start();
    await vi.waitFor(() => expect(root.querySelector("tbody tr")).not.toBeNull());
    expect(actions).toEqual([20]);
    peak = 0;
    for (const id of [12, 13, 14, 15]) {
      root.querySelector<HTMLElement>(`[data-menu-id="${id}"]`)!.dispatchEvent(new Event("pointerover", { bubbles: true }));
      await vi.advanceTimersByTimeAsync(90);
    }
    expect(actions).toEqual([20, 21]);
    release();
    await vi.advanceTimersByTimeAsync(1500);
    expect(peak).toBe(1);
    expect(actions).toEqual([20, 21, 24]);
  } finally {
    vi.useRealTimers();
  }
});

test("menu icon selection and reset retain mounted-form focus, save/readback and discard", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const view = structuredClone(description);
  Object.assign(view.fields["test.note"], { web_icon: { type: "char", label: "Web Icon", required: false, readonly: false, relation: null, relation_field: null, selection: null } });
  view.views[0]!.nodes[0]!.children!.push(2);
  view.views[0]!.nodes.push({ kind: "field", force_save: false, field: { model: "test.note", name: "web_icon" }, widget: "web.menu_icon", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Web Icon" });
  let saved: unknown = "lucide:settings";
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body)) as Record<string, unknown>;
    let value: unknown;
    if (path.endsWith("erp_load_web_menus")) value = { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] };
    else if (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) value = actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, view);
    else if (path.endsWith("get_views")) value = view;
    else if (path.endsWith("onchange")) value = { value: {} };
    else if (path.endsWith("web_save")) {
      saved = (body["vals"] as Record<string, unknown>)["web_icon"];
      value = [{ id: 5, name: "Menu", web_icon: saved }];
    } else value = [{ id: 5, name: "Menu", web_icon: saved }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  const toggle = (): HTMLButtonElement => root.querySelector<HTMLButtonElement>(".menu-icon-toggle")!;
  const selectLibrary = async (): Promise<void> => {
    toggle().click();
    const choice = root.querySelector<HTMLButtonElement>('[data-icon-choice="library"]')!;
    choice.focus(); choice.click();
    await vi.waitFor(() => expect(root.querySelector(".menu-icon-preview svg")?.getAttribute("data-icon")).toBe("library"));
    expect(document.activeElement).toBe(toggle());
  };
  await selectLibrary();
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(root.textContent).toContain("Record saved."));
  expect(saved).toBe("lucide:library");
  const reset = root.querySelector<HTMLButtonElement>(".menu-icon-reset")!;
  reset.focus(); reset.click();
  await vi.waitFor(() => expect(root.querySelector(".menu-icon-preview svg")?.getAttribute("data-icon")).toBe("layout-grid"));
  expect(document.activeElement).toBe(toggle());
  [...root.querySelectorAll("button")].find(button => button.textContent === "Discard")!.click();
  await vi.waitFor(() => expect(root.querySelector(".menu-icon-preview svg")?.getAttribute("data-icon")).toBe("library"));
  expect(document.querySelector("dialog")).toBeNull();
  expect(saved).toBe("lucide:library");
});

test("cold navigation uses one metadata bundle and no separate action or view RPC", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=11");
  const calls = metadataServer();
  const root = host(); app = new HumanApp(root, bootstrap);
  await app.start();
  expect(root.querySelector("tbody tr")).not.toBeNull();
  expect(count(calls, "erp_load_web_action_views")).toBe(1);
  expect(count(calls, "erp_load_web_action")).toBe(0);
  expect(count(calls, "get_views")).toBe(0);
  expect(count(calls, "web_search_read")).toBe(1);
});

test("navigation joins a bundle that menu prefetch is still loading", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    history.replaceState(null, "", "/erp");
    let release!: () => void;
    const held = new Promise<void>(resolve => { release = resolve; });
    const calls = metadataServer("test.note", () => held);
    const root = host(); app = new HumanApp(root, bootstrap); await app.start();
    const tile = root.querySelector<HTMLElement>('.app-tile[data-menu-id="10"]')!;
    tile.dispatchEvent(new Event("pointerover", { bubbles: true }));
    await vi.advanceTimersByTimeAsync(200);
    expect(count(calls, "erp_load_web_action_views")).toBe(1);
    tile.click();
    await vi.advanceTimersByTimeAsync(1);
    expect(count(calls, "erp_load_web_action_views")).toBe(1);
    release();
    await vi.waitFor(() => expect(root.querySelector("tbody tr")).not.toBeNull());
    expect(count(calls, "erp_load_web_action_views")).toBe(1);
    expect(count(calls, "get_views")).toBe(0);
  } finally { vi.useRealTimers(); }
});

test("a failed bundle is forgotten so a subsequent navigation can retry", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=11");
  let deny = true;
  const calls = metadataServer("test.note", async () => { if (deny) throw new Error("Bundle fixture unavailable"); });
  const root = host(); app = new HumanApp(root, bootstrap);
  await expect(app.start()).rejects.toMatchObject({ kind: "network", message: "Unable to connect. Check your connection and try again." });
  deny = false;
  await app.start();
  expect(root.querySelector("tbody tr")).not.toBeNull();
  expect(count(calls, "erp_load_web_action_views")).toBe(2);
  expect(count(calls, "get_views")).toBe(0);
});

test("invalidation while a bundle is loading prevents late view cache publication", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=11");
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  const calls = metadataServer("test.note", () => held);
  const root = host(); app = new HumanApp(root, bootstrap);
  const start = app.start();
  await vi.waitFor(() => expect(count(calls, "erp_load_web_action_views")).toBe(1));
  // This is the same cache clear used by the metadata-write callback.
  app["metadata"].clear();
  release();
  await start;
  expect(count(calls, "get_views")).toBe(0);
  expect(count(calls, "erp_load_web_action_views")).toBe(2);
  root.querySelector<HTMLElement>("tbody tr")!.click();
  await vi.waitFor(() => expect(root.querySelector(".record-form")).not.toBeNull());
  expect(count(calls, "erp_load_web_action_views")).toBe(2);
});

test("two bundles sharing views preserve the first published companion when completing in reverse order", async () => {
  const pending = new Map<number, (value: unknown) => void>();
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    calls.push(path);
    const body = JSON.parse(String(init.body)) as { ids: number[] };
    const value = await new Promise(resolve => pending.set(body.ids[0]!, resolve));
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  app = new HumanApp(host(), bootstrap);
  const first = app["windowAction"](20), second = app["windowAction"](21);
  await vi.waitFor(() => expect(pending.size).toBe(2));
  const bundle = (id: number, version: string) => ({
    action: { wire_major: 1, id, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null },
    views: { ...description, description_version: version }
  });
  pending.get(21)!(bundle(21, "second")); await second;
  pending.get(20)!(bundle(20, "first")); await first;
  const views = await app["viewDescriptions"]("test.note", [[2,"list"],[1,"form"]]);
  expect(views).toMatchObject({ description_version: "second" });
  expect(calls).toHaveLength(2);
});

test.each([
  ["extra", "validation", "Invalid action and view metadata bundle."],
  ["missing", "validation", "Invalid action and view metadata bundle."],
  ["id", "unsupported", "Unsupported or mismatched window action."],
  ["views", "unsupported", "The application changed. Reload to use the current version."]
])("malformed %s bundle is refused without publishing either cache key and can retry", async (fault, kind, message) => {
  let malformed = true;
  const fetch = vi.fn(async () => {
    const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null };
    const bundle: Record<string, unknown> = { action, views: description };
    if (malformed) {
      if (fault === "extra") bundle["extra"] = true;
      if (fault === "missing") delete bundle["views"];
      if (fault === "id") action.id = 21;
      if (fault === "views") bundle["views"] = { ...description, wire_major: 2 };
    }
    return new Response(JSON.stringify(bundle), { headers: { "Content-Type": "application/json" } });
  });
  vi.stubGlobal("fetch", fetch);
  app = new HumanApp(host(), bootstrap);
  await expect(app["windowAction"](20)).rejects.toMatchObject({ kind, message });
  expect(app["metadata"].size).toBe(0);
  malformed = false;
  expect(await app["windowAction"](20)).toMatchObject({ id: 20 });
  expect(app["metadata"].size).toBe(2);
  expect(fetch).toHaveBeenCalledTimes(2);
});

/// (partial: two real action cache identities preserve owned category and custom page defaults)
test("actions sharing the same views keep their own search category and page limit", async () => {
  const calls: number[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body));
    if (path.endsWith("get_views")) return new Response(JSON.stringify(description), { headers: { "Content-Type": "application/json" } });
    const id = body.ids[0]; calls.push(id);
    const views = structuredClone(description) as unknown as { views: { type: string; nodes: Record<string, unknown>[] }[]; fields: Record<string, Record<string, unknown>> };
    views.fields["test.note"]!["priority"] = { type: "selection", label: "Priority", required: false, readonly: false, selection: [["normal", "Normal"], ["high", "High"]], relation: null, relation_field: null };
    Object.assign(views.views.find(view => view.type === "list")!.nodes[0]!, { page_limit: id === 20 ? 75 : 50, search_panel: { kind: "selection", field: { model: "test.note", name: "priority" }, label: id === 20 ? "First category" : "Second category", enable_counters: true, expand: true, default_value: null, notice: null } });
    return new Response(JSON.stringify({ action: { wire_major: 1, id, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null }, views }), { headers: { "Content-Type": "application/json" } });
  }));
  app = new HumanApp(host(), bootstrap);
  await Promise.all([app["windowAction"](20), app["windowAction"](21)]);
  const selected = [[2, "list"], [1, "form"]];
  const first = describeViews(await app["viewDescriptions"]("test.note", selected, {}, undefined, 20), "test.note", "r1");
  const second = describeViews(await app["viewDescriptions"]("test.note", selected, {}, undefined, 21), "test.note", "r1");
  expect(first.views.get("list")).toMatchObject({ pageLimit: 75, searchPanel: { label: "First category" } });
  expect(second.views.get("list")).toMatchObject({ pageLimit: 50, searchPanel: { label: "Second category" } });
  const standalone = describeViews(await app["viewDescriptions"]("test.note", selected), "test.note", "r1");
  expect(standalone.views.get("list")?.pageLimit).toBeUndefined();
  expect(standalone.views.get("list")?.searchPanel).toBeUndefined();
  expect(calls).toEqual([20, 21]);
});

/// (partial: action cache retains the complete opaque form DTO, independent of list metadata)
test("action companion cache preserves every form fragment without projecting the arena", async () => {
  const complete = structuredClone(description) as unknown as { views: { type: string; nodes: unknown[] }[] };
  const field = description.views[0]!.nodes[1]!;
  complete.views.find(view => view.type === "form")!.nodes = [
    { kind: "form", record_mode: "record", children: [1, 3, 8] },
    { kind: "header", children: [2] },
    { kind: "button", name: "approve", button_type: "object", label: "Approve", variant: "primary", confirm: null, modifiers: { readonly: null, required: null, invisible: null } },
    { kind: "sheet", children: [4, 5] },
    field,
    { kind: "notebook", children: [6] },
    { kind: "page", name: "details", label: "Details", children: [7] },
    { kind: "separator", label: "Additional details" },
    { kind: "footer", children: [9] },
    { kind: "cancel_button", label: "Close", variant: "secondary", modifiers: { readonly: null, required: null, invisible: null } }
  ];
  let bundleReads = 0;
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    expect(path.endsWith("erp_load_web_action_views")).toBe(true); bundleReads++;
    return new Response(JSON.stringify({ action: { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null }, views: complete }), { headers: { "Content-Type": "application/json" } });
  }));
  app = new HumanApp(host(), bootstrap);
  await Promise.all([app["windowAction"](20), app["windowAction"](20)]);
  const selected = [[2, "list"], [1, "form"]];
  const first = await app["viewDescriptions"]("test.note", selected, {}, undefined, 20) as typeof complete;
  expect(first).toEqual(complete);
  first.views.find(view => view.type === "form")!.nodes.pop();
  expect(await app["viewDescriptions"]("test.note", selected, {}, undefined, 20)).toEqual(complete);
  expect(bundleReads).toBe(1);
});

test("office sidebar keeps same-root route ancestry open through navigation and history", async () => {
  history.replaceState(null, "", "/erp/action-100?menu=10");
  const longLabel = "Administration — permissions, models and interface settings";
  const menus = { wire_major: 1, registry_version: "r1", roots: [1], nodes: [
    { module: null, id: 1, parent_id: null, label: longLabel, action_id: null, children: [2, 3], icon: null },
    { module: null, id: 2, parent_id: 1, label: "Structure", action_id: null, children: [10], icon: null },
    { module: null, id: 3, parent_id: 1, label: "Interface", action_id: null, children: [11], icon: null },
    { module: null, id: 10, parent_id: 2, label: "Models", action_id: 100, children: [], icon: null },
    { module: null, id: 11, parent_id: 3, label: "Views", action_id: 101, children: [], icon: null }
  ] };
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const args = JSON.parse(String(init.body));
    const value = path.endsWith("erp_load_web_menus") ? menus
      : path.endsWith("erp_load_web_action_views") ? { action: { wire_major: 1, id: args.ids[0], name: "Records", model: "test.note", target: "current", views: [{ id: 2, type: "list" }], domain: null, context: null }, views: description }
      : path.endsWith("web_search_read") ? { length: 0, records: [] } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const sidebar = root.querySelector<HTMLElement>(".office-navigation")!;
  const models = sidebar.querySelector<HTMLButtonElement>('[data-menu-id="10"]')!;
  const views = sidebar.querySelector<HTMLButtonElement>('[data-menu-id="11"]')!;
  expect(sidebar.querySelector("summary span")?.textContent).toBe(longLabel);
  expect(models.getAttribute("aria-current")).toBe("page");
  expect(views.closest("details")?.open).toBe(false);
  views.click();
  await vi.waitFor(() => expect(views.getAttribute("aria-current")).toBe("page"));
  expect(views.closest("details")?.open).toBe(true);
  expect(models.hasAttribute("aria-current")).toBe(false);
  models.closest("details")!.open = false;
  history.back();
  await vi.waitFor(() => expect(models.getAttribute("aria-current")).toBe("page"));
  expect(models.closest("details")?.open).toBe(true);
  expect(views.hasAttribute("aria-current")).toBe(false);
  models.focus();
  expect(document.activeElement).toBe(models);
});

/// (partial: authorized header menu search, keyboard navigation and existing dirty guard)
test("Office header searches actual menus and retains the dirty-form navigation decision", async () => {
  document.documentElement.dataset["officeTheme"] = "office-blue";
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    const actionId = body.ids?.[0];
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10, 11], nodes: [
      { module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null },
      { module: null, id: 11, parent_id: null, label: "Actions", action_id: 21, children: [], icon: null }
    ] } : path.endsWith("erp_load_web_action_views") ? actionMetadata(path, { wire_major: 1, id: actionId, name: actionId === 20 ? "Notes" : "Actions", model: "test.note", target: "current", views: [{ id: actionId === 20 ? 1 : 2, type: actionId === 20 ? "form" : "list" }], domain: null, context: null }, description)
      : path.endsWith("web_read") ? [{ id: 5, name: "Existing title" }]
      : path.endsWith("onchange") ? { value: {} }
      : path.endsWith("web_search_read") ? { length: 0, records: [] } : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  // Match the compiled <main tabindex="-1"> so successful navigation really moves focus.
  root.querySelector<HTMLElement>('[data-client-content]')!.tabIndex = -1;
  app = new HumanApp(root, bootstrap); await app.start();
  const search = root.querySelector<HTMLInputElement>('.office-header-search input[aria-label="Search apps and menus"]');
  expect(search).not.toBeNull();
  const title = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  title.value = "Keep this edit"; title.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  const choose = (): void => {
    search!.focus(); search!.value = "Actions"; search!.dispatchEvent(new Event("input"));
    const option = root.querySelector<HTMLButtonElement>('.office-header-search [role="option"]')!;
    expect(option.textContent).toBe("Actions"); expect(option.classList.contains("has-action-icon")).toBe(false);
    search!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    search!.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  };
  choose();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
  [...document.querySelectorAll<HTMLButtonElement>(".choice-dialog button")].find(button => button.textContent === "Cancel")!.click();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).toBeNull());
  expect(window.location.pathname).toBe("/erp/action-20");
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Keep this edit");
  expect(document.activeElement).toBe(search);
  choose();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
  [...document.querySelectorAll<HTMLButtonElement>(".choice-dialog button")].find(button => button.textContent === "Discard")!.click();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/erp/action-21"));
  expect(root.querySelector('.office-header-search [role="listbox"]')?.hasAttribute("hidden")).toBe(true);
});

test('navigation cannot dispose the source while a child modal owns an operation',async()=>{
  history.replaceState(null,'','/erp/action-20?id=5&view_type=form');
  vi.stubGlobal('fetch',vi.fn(async(path:string)=>new Response(JSON.stringify(path.endsWith('erp_load_web_menus')?{wire_major:1,registry_version:'r1',roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:'Notes',action_id:20,children:[],icon:null}]}:(path.endsWith('erp_load_web_action')||path.endsWith('erp_load_web_action_views'))?actionMetadata(path,{wire_major:1,id:20,name:'Notes',model:'test.note',target:'current',views:[{id:1,type:'form'}],domain:null,context:null},description):path.endsWith('get_views')?description:[{id:5,name:'Source'}]),{headers:{'Content-Type':'application/json'}})));
  const root=host();app=new HumanApp(root,bootstrap);await app.start();
  const modal=app.messages.dialogs.open({title:'Related operation',body:'Keep the source until this modal is closed.',dismiss:()=>false});
  const before=location.href;root.querySelector<HTMLButtonElement>(".brand")!.click();await new Promise<void>(resolve=>setTimeout(resolve,0));expect(location.href).toBe(before);expect(root.querySelector('.record-form')).not.toBeNull();expect(modal.dialog.isConnected).toBe(true);
  modal.close();root.querySelector<HTMLButtonElement>(".brand")!.click();await vi.waitFor(()=>expect(root.querySelector('.record-form')).toBeNull());
});

test('loaded-empty help is distinct from pending reads and load failures',async()=>{
 history.replaceState(null,'','/erp/action-20?view_type=list');
 let readStarted=false, fail=false, complete:((value:Response)=>void)|undefined;
 vi.stubGlobal('fetch',vi.fn(async(path:string)=>{
  if(path.endsWith('web_search_read')){readStarted=true;if(fail)return new Response('{"message":"List failed"}',{status:500,headers:{'Content-Type':'application/json'}});return new Promise<Response>(resolve=>{complete=resolve;});}
  const value=path.endsWith('erp_load_web_menus')?{wire_major:1,registry_version:'r1',roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:'Notes',action_id:20,children:[],icon:null}]}:(path.endsWith('erp_load_web_action')||path.endsWith('erp_load_web_action_views'))?actionMetadata(path,{wire_major:1,id:20,name:'Notes',model:'test.note',target:'current',views:[{id:2,type:'list'}],domain:null,context:null},description):path.endsWith('get_views')?description:[];
  return new Response(JSON.stringify(value),{headers:{'Content-Type':'application/json'}});
 }));
 const root=host();app=new HumanApp(root,bootstrap);const started=app.start();
 await vi.waitFor(()=>expect(readStarted).toBe(true));expect(root.querySelector('.message-no-content')).toBeNull();
 complete!(new Response('{"length":0,"records":[]}',{headers:{'Content-Type':'application/json'}}));await started;
 expect(root.querySelector('.message-no-content')?.getAttribute('role')).toBe('alert');expect(root.querySelector('.message-no-content h2')?.textContent).toBe('No Notes records yet');
 expect(root.querySelector('table[aria-label="Records"]')).not.toBeNull();
 expect(root.querySelectorAll('table[aria-label="Records"] tbody tr').length).toBe(0);
 app.dispose();root.remove();fail=true;history.replaceState(null,'','/erp/action-20?view_type=list');const errorRoot=host();app=new HumanApp(errorRoot,bootstrap);
 await expect(app.start()).rejects.toMatchObject({kind:'server',message:'List failed'});expect(errorRoot.querySelector('.message-no-content')).toBeNull();
});

test('disposing a pending bootstrap suppresses late transport presentation and view mounting',async()=>{
 let finish:((response:Response)=>void)|undefined;
 vi.stubGlobal('fetch',vi.fn(()=>new Promise<Response>(resolve=>{finish=resolve;})));
 const root=host();app=new HumanApp(root,bootstrap);const starting=app.start();app.dispose();
 finish!(new Response('{"wire_major":1,"registry_version":"r1","roots":[],"nodes":[]}',{headers:{'Content-Type':'application/json'}}));
 await expect(starting).resolves.toBeUndefined();expect(root.querySelector('.home-grid')).toBeNull();expect(root.querySelector('.client-network-status')).toBeNull();
});

test('mounted app presents offline and safe-read recovery states without replaying writes',async()=>{
 history.replaceState(null,'','/erp/action-20?id=5&view_type=form');
 let offline=false, refreshing=false, writes=0, finish:((value:Response)=>void)|undefined;
 const json=(value:unknown,status=200)=>new Response(JSON.stringify(value),{status,headers:{'Content-Type':'application/json'}});
 vi.stubGlobal('fetch',vi.fn(async(path:string)=>{
  if(path.endsWith('client-error'))return json({});
  if(path.endsWith('web_save'))writes++;
  if(path.endsWith('onchange')){if(offline)throw new TypeError('Connection lost');return json({value:{}});}
  if(path.endsWith('web_read')&&refreshing)return new Promise<Response>(resolve=>{finish=resolve;});
  return json(path.endsWith('erp_load_web_menus')?{wire_major:1,registry_version:'r1',roots:[10],nodes:[{ module: null,id:10,parent_id:null,label:'Notes',action_id:20,children:[],icon:null}]}:(path.endsWith('erp_load_web_action')||path.endsWith('erp_load_web_action_views'))?actionMetadata(path,{wire_major:1,id:20,name:'Notes',model:'test.note',target:'current',views:[{id:1,type:'form'}],domain:null,context:null},description):path.endsWith('get_views')?description:[{id:5,name:'Source'}]);
 }));
 const root=host();app=new HumanApp(root,bootstrap);await app.start();offline=true;
 const input=root.querySelector<HTMLInputElement>('input[name="name"]')!;input.value='Entered';input.dispatchEvent(new Event('change'));
 const state=()=>root.querySelector<HTMLElement>('.client-network-status')?.dataset['state'];
 await vi.waitFor(()=>expect(state()).toBe('offline'));document.querySelector<HTMLButtonElement>('[data-error-close]')!.click();
 const reload=async()=>{refreshing=true;root.querySelector<HTMLButtonElement>('.form-reload-btn')!.click();await vi.waitFor(()=>expect(document.querySelector('.message-dialog-footer')).not.toBeNull());[...document.querySelectorAll<HTMLButtonElement>('.message-dialog-footer button')].find(b=>b.textContent==='Reload')!.click();await vi.waitFor(()=>expect(state()).toBe('syncing'));};
 await reload();finish!(json({message:'Read failed'},503));await vi.waitFor(()=>expect(state()).toBe('error'));document.querySelector<HTMLButtonElement>('[data-error-close]')!.click();
 await reload();finish!(json([{id:5,name:'Authoritative value'}]));await vi.waitFor(()=>expect(state()).toBe('online'));expect(root.querySelector<HTMLElement>('.client-network-status')?.hidden).toBe(true);expect(writes).toBe(0);
});

test('mounted app settles cancelled recovery reads while another read remains active', async () => {
 let firstCall=true;
 const fetcher=vi.fn((_path: RequestInfo | URL, options?: RequestInit): Promise<Response> => {
  if(firstCall){firstCall=false;return Promise.reject(new TypeError('Connection lost'));}
  return new Promise((_resolve,reject)=>options?.signal?.addEventListener('abort',()=>reject(new DOMException('Read cancelled','AbortError')),{once:true}));
 });
 vi.stubGlobal('fetch',fetcher);
 const root=host();app=new HumanApp(root,bootstrap);
 // Exercise the actual app-owned transport observer without routing an unrelated view.
 const rpc=(app as unknown as {rpc: import('../src/client/transport.js').RpcClient}).rpc;
 const state=()=>root.querySelector<HTMLElement>('.client-network-status')?.dataset['state'];
 await expect(rpc.call('test.note','web_read',{})).rejects.toMatchObject({kind:'network',message:'Unable to connect. Check your connection and try again.'});
 expect(state()).toBe('offline');
 const first=new AbortController(),second=new AbortController();
 const a=rpc.call('test.note','web_read',{}, {signal:first.signal}).catch(error=>error);
 const b=rpc.call('test.note','web_read',{}, {signal:second.signal}).catch(error=>error);
 expect(state()).toBe('syncing');first.abort();expect(await a).toMatchObject({name:'AbortError'});expect(state()).toBe('syncing');
 second.abort();expect(await b).toMatchObject({name:'AbortError'});expect(state()).toBe('offline');
 expect(fetcher).toHaveBeenCalledTimes(3);expect(fetcher.mock.calls.every(([path])=>String(path).endsWith('/web_read'))).toBe(true);
});


test.each([{ date_format: "%j" }, { time_format: "%f" }])("unsupported editor format %j cannot prevent non-date navigation and save", async format => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body)) as Record<string, unknown>; calls.push({ path, body });
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, description)
      : path.endsWith("get_views") ? description
      : path.endsWith("onchange") ? { value: { name: objectName(body["values"]) } }
      : path.endsWith("web_save") ? [{ id: 5, name: objectName(body["vals"]) }]
      : [{ id: 5, name: "Note" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, { ...bootstrap, locale: { ...bootstrap.locale, ...format } }); await app.start();
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!; expect(input.value).toBe("Note");
  input.value = "Correct language settings"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(calls.some(call => call.path.endsWith("onchange"))).toBe(true));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(root.textContent).toContain("Record saved."));
  expect(calls.find(call => call.path.endsWith("web_save"))?.body["vals"]).toEqual({ name: "Correct language settings" });
});

test.each([
  { format: { date_format: "%j" }, directive: "%j", errors: 2, dayDisplay: "2026-11-03" },
  { format: { time_format: "%f" }, directive: "%f", errors: 1, dayDisplay: "11/03/2026" },
])("mounted mixed form keeps unsupported $directive settings inline without blocking sibling edit and save", async ({ format, directive, errors, dayDisplay }) => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const dateField = (type: "date" | "datetime", label: string) => ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null });
  const fieldNode = (field: string, widget: string, label: string) => ({ kind: "field", force_save: false, field: { model: "test.note", name: field }, widget, options: {}, modifiers: { readonly: null, required: null, invisible: null }, label });
  const mixedDescription = {
    ...description,
    fields: { "test.note": { ...description.fields["test.note"], day: dateField("date", "Day"), appointment: dateField("datetime", "Appointment") } },
    views: [{ ...description.views[0]!, nodes: [
      { kind: "form", record_mode: "record", children: [1, 2, 3] },
      fieldNode("name", "char", "Name"), fieldNode("day", "date", "Day"), fieldNode("appointment", "datetime", "Appointment"),
    ] }],
  };
  const values = { id: 5, name: "Note", day: "2026-11-03", appointment: "2026-11-03 10:15:00" };
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = JSON.parse(String(init.body)) as Record<string, unknown>; calls.push({ path, body });
    const value = path.endsWith("client-error") ? {}
      : path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, mixedDescription)
      : path.endsWith("get_views") ? mixedDescription
      : path.endsWith("onchange") ? { value: { name: objectName(body["values"]) } }
      : path.endsWith("web_save") ? [{ ...values, name: objectName(body["vals"]) }]
      : [values];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, { ...bootstrap, locale: { ...bootstrap.locale, ...format } }); await app.start();
  const inlineErrors = (): Element[] => [...root.querySelectorAll('.editor-validation:not([hidden])')];
  expect(inlineErrors().map(error => error.textContent)).toEqual(Array.from({ length: errors }, () => `Unsupported date/time format directive: ${directive}.`));
  expect(document.querySelector("dialog")).toBeNull(); expect(document.querySelector(".message-popover")).toBeNull();
  expect(calls.filter(call => call.path.endsWith("onchange"))).toEqual([]);
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!; expect(input.disabled).toBe(false); expect(input.value).toBe("Note");
  input.focus(); input.value = "Repair language"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(calls.filter(call => call.path.endsWith("onchange"))).toHaveLength(1));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  expect(document.querySelector("dialog")).toBeNull();
  expect(calls.find(call => call.path.endsWith("onchange"))?.body["values"]).toMatchObject({ name: "Repair language", day: values.day, appointment: values.appointment });
  [...root.querySelectorAll("button")].find(button => button.textContent === "Save")!.click();
  await vi.waitFor(() => expect(root.textContent).toContain("Record saved."));
  expect(calls.filter(call => call.path.endsWith("web_save"))).toHaveLength(1);
  expect(calls.find(call => call.path.endsWith("web_save"))?.body["vals"]).toEqual({ name: "Repair language" });
  expect(root.querySelector<HTMLInputElement>('input[name="day"]')?.value).toBe(dayDisplay);
  expect(root.querySelector<HTMLInputElement>('input[name="appointment"]')?.value).toBe(values.appointment);
  expect(document.querySelector("dialog")).toBeNull(); expect(inlineErrors()).toHaveLength(errors);
  expect(calls.filter(call => call.path.endsWith("onchange"))).toHaveLength(1);
});

/// (partial: form hotkeys route through existing save and discard handlers)
test("form hotkeys save once from an editor and discard through the existing state", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, false));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start(); await editFirstNote(root);
  const hotkey = (key: string, repeat = false): void => {
    const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
    input.dispatchEvent(new KeyboardEvent("keydown", { key, code: `Key${key.toUpperCase()}`, altKey: true, repeat, bubbles: true, cancelable: true }));
  };
  hotkey("s"); hotkey("s", true);
  await vi.waitFor(() => expect(log.saved).toEqual([{ name: "Changed" }]));
  await vi.waitFor(() => expect(root.querySelector(".status-badge")?.textContent).toBe("Saved"));
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!; input.value = "Discard me"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  hotkey("j"); await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed"));
  expect(log.saved).toHaveLength(1);
});

/// (partial: failed shortcut save keeps dirty edits and native modal blocks background shortcuts)
test("a failed shortcut save preserves edits and an active modal blocks background save", async () => {
  history.replaceState(null, "", "/erp/action-20"); const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, true));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start(); await editFirstNote(root);
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "s", code: "KeyS", altKey: true, bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(log.saved).toHaveLength(1));
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed");
  const dialog = document.createElement("dialog"); document.body.append(dialog); dialog.showModal();
  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "s", code: "KeyS", altKey: true, bubbles: true, cancelable: true }));
  await Promise.resolve(); expect(log.saved).toHaveLength(1); dialog.remove();
});

/// (partial: save admission spans deferred editor commit)
test("pending editor commit owns save and discard before the first await", async () => {
  history.replaceState(null, "", "/erp/action-20"); const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, false)); const root = host(); app = new HumanApp(root, bootstrap); await app.start(); await editFirstNote(root);
  let release!: () => void; const pending = new Promise<void>(resolve => { release = resolve; });
  vi.spyOn(RecordForm.prototype, "needsCommit").mockReturnValue(true);
  const commit = vi.spyOn(RecordForm.prototype, "commit").mockReturnValue(pending);
  root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
  root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();
  root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
  root.querySelector<HTMLInputElement>('input[name="name"]')!.dispatchEvent(new KeyboardEvent("keydown", {key:"j",code:"KeyJ",altKey:true,bubbles:true,cancelable:true}));
  expect(commit).toHaveBeenCalledOnce(); expect(log.saved).toHaveLength(0);
  expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed");
  release(); await vi.waitFor(() => expect(log.saved).toEqual([{name:"Changed"}]));
});

/// (partial: asynchronous failures remain visible after application disposal)
test("a late command rejection reaches the browser error channel after app disposal", async () => {
  history.replaceState(null, "", "/erp/action-20"); const log = { saved: [] as unknown[], reads: [] as number[] };
  vi.stubGlobal("fetch", pagerSaveFetch(log, false)); const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  const report = vi.fn(), failure = new Error("late command rejection"); vi.stubGlobal("reportError", report);
  let reject!: (error: unknown) => void;
  const pending = new Promise<void>((_resolve, no) => { reject = no; });
  app.run(() => pending); app.dispose(); reject(failure);
  await vi.waitFor(() => expect(report).toHaveBeenCalledExactlyOnceWith(failure));
});

/// (partial: scalar input is committed by keyboard Save without a prior field departure)
test("Alt S commits raw scalar text and Alt J discards it without a prior change event", async () => {
  history.replaceState(null,"","/erp/action-20");const log={saved:[] as unknown[],reads:[] as number[]};
  vi.stubGlobal("fetch",pagerSaveFetch(log,false));const root=host();app=new HumanApp(root,bootstrap);await app.start();await editFirstNote(root);
  // First discard returns to a clean form, whose visible Save control is hidden.
  root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
  const input=root.querySelector<HTMLInputElement>('input[name="name"]')!;input.focus();input.value="Raw keyboard value";input.dispatchEvent(new Event("input"));
  input.dispatchEvent(new KeyboardEvent("keydown",{key:"s",code:"KeyS",altKey:true,bubbles:true,cancelable:true}));
  await vi.waitFor(()=>expect(log.saved).toEqual([{name:"Raw keyboard value"}]));
  await vi.waitFor(()=>expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  const next=root.querySelector<HTMLInputElement>('input[name="name"]')!;next.focus();next.value="Discard raw";next.dispatchEvent(new Event("input"));
  next.dispatchEvent(new KeyboardEvent("keydown",{key:"j",code:"KeyJ",altKey:true,bubbles:true,cancelable:true}));
  await vi.waitFor(()=>expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed"));
  expect(log.saved).toHaveLength(1);
});

/// (partial: keyboard Save excludes conflicting clicks during a preceding scalar commit)
test("keyboard Save owns conflicting clicks while a scalar change waits for editor commit", async () => {
  history.replaceState(null,"","/erp/action-20");const log={saved:[] as unknown[],reads:[] as number[]};
  vi.stubGlobal("fetch",pagerSaveFetch(log,false));const root=host();app=new HumanApp(root,bootstrap);await app.start();await editFirstNote(root);
  const input=root.querySelector<HTMLInputElement>('input[name="name"]')!;input.focus();input.value="Raw held value";input.dispatchEvent(new Event("input"));
  let release!:()=>void;const pending=new Promise<void>(resolve=>{release=resolve;});
  vi.spyOn(RecordForm.prototype,"needsCommit").mockReturnValue(true);const commit=vi.spyOn(RecordForm.prototype,"commit").mockReturnValue(pending);
  input.dispatchEvent(new KeyboardEvent("keydown",{key:"s",code:"KeyS",altKey:true,bubbles:true,cancelable:true}));
  try {
    root.querySelector<HTMLButtonElement>(".status-btn-save")!.click();root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
    expect(commit).toHaveBeenCalledOnce();expect(log.saved).toHaveLength(0);expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Raw held value");
  } finally {release();}
  await vi.waitFor(()=>expect(log.saved).toEqual([{name:"Raw held value"}]));
});

async function externalScalarHotkeys(kind:"header"|"aside"):Promise<void> {
  history.replaceState(null,"","/erp/action-20?id=5&view_type=form");document.documentElement.dataset["officeTheme"]="office-blue";
  const log={saved:[] as unknown[],reads:[] as number[]};const base=pagerSaveFetch(log,false);
  const changed=JSON.parse(JSON.stringify(description)) as {views:{nodes:unknown[]}[]};
  changed.views[0]!.nodes=[{kind:"form",record_mode:"record",children:[1]},{kind,children:[2]},description.views[0]!.nodes[1]];
  vi.stubGlobal("fetch",async(path:string,init:RequestInit)=>{
    if(path.endsWith("get_views"))return new Response(JSON.stringify(changed),{headers:{"Content-Type":"application/json"}});
    const response=await base(path,init);
    if(path.endsWith("erp_load_web_action_views")){const value=await response.json() as Record<string,unknown>;value["views"]=changed;return new Response(JSON.stringify(value),{headers:{"Content-Type":"application/json"}});}
    return response;
  });
  const root=host();app=new HumanApp(root,bootstrap);await app.start();
  const input=root.querySelector<HTMLInputElement>('input[name="name"]')!;expect(input).not.toBeNull();expect(root.querySelector("form")!.contains(input)).toBe(false);
  input.focus();input.value=`Raw ${kind}`;input.dispatchEvent(new Event("input"));input.dispatchEvent(new KeyboardEvent("keydown",{key:"s",code:"KeyS",altKey:true,bubbles:true,cancelable:true}));
  await vi.waitFor(()=>expect(log.saved).toEqual([{name:`Raw ${kind}`} ]));
  await vi.waitFor(()=>expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
  const current=root.querySelector<HTMLInputElement>('input[name="name"]')!;current.focus();current.value="Discard external";current.dispatchEvent(new Event("input"));current.dispatchEvent(new KeyboardEvent("keydown",{key:"j",code:"KeyJ",altKey:true,bubbles:true,cancelable:true}));
  await vi.waitFor(()=>expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.value).toBe("Changed"));expect(log.saved).toHaveLength(1);
}
// Header scalar controls are not admitted; do not widen metadata merely to construct a shortcut fixture.
test("raw scalar header fields retain their canonical admission rejection",()=>{
  const changed=JSON.parse(JSON.stringify(description)) as {views:{nodes:unknown[]}[]};
  changed.views[0]!.nodes=[{kind:"form",record_mode:"record",children:[1]},{kind:"header",children:[2]},description.views[0]!.nodes[1]];
  expect(()=>describeViews(changed,"test.note","r1")).toThrowError(expect.objectContaining({kind:"unsupported",message:"Header fields require the statusbar widget."}));
});
/// (partial: aside-owned scalar shortcut commit and discard)
test("keyboard Save and Discard include external aside scalar fields",async()=>externalScalarHotkeys("aside"));

/// (partial: mounted editor-local text blocks recovery before RecordState commits it)
test("panel compatibility recovery never discards an editor-local draft", async () => {
  history.replaceState(null,"","/erp/action-20");const log={saved:[] as unknown[],reads:[] as number[]};
  vi.stubGlobal("fetch",pagerSaveFetch(log,false));const root=host();app=new HumanApp(root,bootstrap);await app.start();await editFirstNote(root);
  root.querySelector<HTMLButtonElement>(".status-btn-discard")!.click();
  const input=root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.focus();input.value="Uncommitted local draft";input.dispatchEvent(new Event("input"));
  const recovery=vi.spyOn(panelRecovery,"attemptPanelRecovery").mockReturnValue(false);
  app.run(async()=>{throw new panelRecovery.PanelCompatibilityError();});
  await vi.waitFor(()=>expect(recovery).toHaveBeenCalledExactlyOnceWith(expect.any(Function),false,expect.any(Function)));
  expect(input.value).toBe("Uncommitted local draft");expect(log.saved).toHaveLength(0);
  expect(document.body.textContent).toContain(panelRecovery.panelCompatibilityMessage);
});

test("window action domain filters list view records via initial domain", async () => {
  history.replaceState(null, "", "/erp/action-25");
  const domains: unknown[] = [];
  const actionDomainDoc = {
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "parent_id" } },
      { kind: "literal", value: { kind: "str", value: "=" } },
      { kind: "literal", value: { kind: "bool", value: false } },
      { kind: "tuple", items: [0, 1, 2] },
      { kind: "list", items: [3] }
    ]
  };
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init?.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      domains.push(body.domain);
      return new Response(JSON.stringify({ length: 1, records: [{ id: 1, name: "Top Level Company" }] }), { headers: { "Content-Type": "application/json" } });
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 25, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 25, name: "Companies", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }], domain: actionDomainDoc, context: null
    }, description) : path.endsWith("get_views") ? description : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(domains.length).toBeGreaterThan(0));
  expect(domains[0]).toEqual([["parent_id", "=", false]]);
});

/// (partial: list arch create, edit, delete attribute parsing and active action control)
test("list view hides New button when view has create=false or context create is falsy", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const listDesc = JSON.parse(JSON.stringify(description));
  listDesc.views[1].nodes[0].create = false;
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null }, listDesc)
      : path.endsWith("get_views") ? listDesc
      : { length: 0, records: [] };
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const buttons = Array.from(root.querySelectorAll<HTMLButtonElement>("button"));
  expect(buttons.some(btn => btn.textContent?.trim() === "New")).toBe(false);
});

/// (partial: form arch create, edit, delete attribute parsing and active action control)
test("form view hides New button when view has create=false or context create is falsy", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const formDesc = JSON.parse(JSON.stringify(description));
  formDesc.views[0].nodes[0].create = false;
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, formDesc)
      : path.endsWith("get_views") ? formDesc
      : [{ id: 5, name: "Existing note" }];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const buttons = Array.from(root.querySelectorAll<HTMLButtonElement>("button"));
  expect(buttons.some(btn => btn.textContent?.trim() === "New")).toBe(false);
});

/// Second review pass: the window action domain sees the same names as a button domain.
test("a window action domain may name uid", async () => {
  history.replaceState(null, "", "/erp/action-25");
  const domains: unknown[] = [];
  const doc = {
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "owner_id" } },
      { kind: "literal", value: { kind: "str", value: "=" } },
      { kind: "name", name: "uid" },
      { kind: "tuple", items: [0, 1, 2] },
      { kind: "list", items: [3] }
    ]
  };
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init?.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_search_read")) {
      domains.push(body.domain);
      return new Response(JSON.stringify({ length: 0, records: [] }), { headers: { "Content-Type": "application/json" } });
    }
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 25, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 25, name: "Mine", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }], domain: doc, context: null
    }, description) : path.endsWith("get_views") ? description : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  await vi.waitFor(() => expect(domains.length).toBeGreaterThan(0));
  expect(domains[0]).toEqual([["owner_id", "=", 2]]);
});

/// Second review pass: the developer menu appears with ?debug=1 and re-opening it disposes the earlier render.
test("the debug menu button shows with debug=1 and a reopen disposes the previous render", async () => {
  history.replaceState(null, "", "/erp?debug=1");
  const disposed = vi.fn();
  registerDebugItemProvider("secondPass", () => [{ id: "probe", type: "component", render: () => ({ node: document.createElement("span"), dispose: disposed }) }]);
  vi.stubGlobal("fetch", vi.fn(async (path: string) => new Response(JSON.stringify(path.endsWith("erp_load_web_menus")
    ? { wire_major: 1, registry_version: "r1", roots: [], nodes: [] } : []), { headers: { "Content-Type": "application/json" } })));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  const toggle = root.querySelector<HTMLButtonElement>(".debug-menu-button");
  expect(toggle).not.toBeNull();
  toggle!.click();
  await vi.waitFor(() => expect(root.querySelector(".debug-menu-dropdown")?.children.length).toBeGreaterThan(0));
  toggle!.click();
  toggle!.click();
  await vi.waitFor(() => expect(disposed).toHaveBeenCalledTimes(1));
});

/// Second review pass: an edit="0" view renders read-only after the first save of a new record and ignores keyboard save.
/// (partial: edit=0 form renders read-only once a new record is saved and ignores keyboard save)
test("an edit=0 form renders read-only after its first save and ignores keyboard save", async () => {
  history.replaceState(null, "", "/erp/action-20?view_type=form");
  const noEdit = JSON.parse(JSON.stringify(description));
  noEdit.views[0].nodes[0].edit = false;
  const saved: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : {};
    if (path.endsWith("web_save")) saved.push(body.vals);
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current",
      views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null
    }, noEdit) : path.endsWith("get_views") ? noEdit
      : path.endsWith("default_get") ? { name: "" }
      : path.endsWith("onchange") ? { value: {} }
      : path.endsWith("web_save") ? [{ id: 7, name: "Fresh" }]
      : path.endsWith("web_read") ? [{ id: 7, name: "Fresh" }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  // While new (!state.id), fields are editable
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  expect(input).not.toBeNull();
  input.focus();
  input.value = "Fresh";
  input.dispatchEvent(new Event("input"));
  // First save works via keyboard shortcut Alt+S
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "s", code: "KeyS", altKey: true, bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(saved).toHaveLength(1));

  // Once saved (record now has id 7), the form must render read-only
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')).toBeNull());
  const readonlyField = root.querySelector(".field-readonly");
  expect(readonlyField).not.toBeNull();
  const readValue = root.querySelector(".field-read-value");
  expect(readValue?.textContent).toBe("Fresh");

  // Status actions (Save/Discard) are hidden for the read-only record
  const statusActions = root.querySelector<HTMLElement>(".status-actions");
  expect(statusActions?.style.display).toBe("none");

  // Keyboard save (Alt+S) of such a record must be ignored and not trigger another save
  root.dispatchEvent(new KeyboardEvent("keydown", { key: "s", code: "KeyS", altKey: true, bubbles: true, cancelable: true }));
  await Promise.resolve();
  expect(saved).toHaveLength(1);

  // Directly attempting a change on RecordState throws validation error because the field is readonly
  const internals = app as unknown as { record: { dirty: boolean; busy: boolean; change(n: string, v: unknown): void }; canLeave(o: { autosave: boolean }): Promise<boolean> };
  expect(() => internals.record.change("name", "Sneaky")).toThrowError(new ClientError("validation", "This field cannot be edited."));
  expect(internals.record.dirty).toBe(false);

  // If a locked record has staged changes, canLeave({ autosave: true }) must discard rather than saving
  ((internals.record as any).changes as Set<string>).add("name");
  expect(internals.record.dirty).toBe(true);
  expect(await internals.canLeave({ autosave: true })).toBe(true);
  expect(internals.record.dirty).toBe(false);
  expect(root.textContent).toContain("This record is read-only; the change was not saved.");
  await Promise.resolve();
  expect(saved).toHaveLength(1);
});

test("the debug menu supports keyboard roving focus, Escape dismissal, and global Escape overlay handling", async () => {
  history.replaceState(null, "", "/erp?debug=1");
  registerDebugItemProvider("debugA11yTest", () => [
    { id: "item1", description: "First Debug Tool", callback: () => {} },
    { id: "item2", description: "Second Debug Tool", callback: () => {} }
  ]);
  vi.stubGlobal("fetch", vi.fn(async (path: string) => new Response(JSON.stringify(path.endsWith("erp_load_web_menus")
    ? { wire_major: 1, registry_version: "r1", roots: [], nodes: [] } : []), { headers: { "Content-Type": "application/json" } })));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();

  const toggle = root.querySelector<HTMLButtonElement>(".debug-menu-button")!;
  const dropdown = root.querySelector<HTMLElement>(".debug-menu-dropdown")!;
  expect(toggle).not.toBeNull();
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(dropdown.style.display).toBe("none");

  toggle.focus();
  toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

  await vi.waitFor(() => expect(dropdown.style.display).toBe("block"));
  expect(toggle.getAttribute("aria-expanded")).toBe("true");

  await vi.waitFor(() => {
    const items = dropdown.querySelectorAll<HTMLButtonElement>(".debug-menu-item");
    expect(items.length).toBeGreaterThan(0);
    expect(document.activeElement).toBe(items[0]);
  });
  const items = dropdown.querySelectorAll<HTMLButtonElement>(".debug-menu-item");

  dropdown.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(document.activeElement).toBe(items[1]);

  dropdown.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(dropdown.style.display).toBe("none");
  expect(document.activeElement).toBe(toggle);

  toggle.click();
  await vi.waitFor(() => expect(dropdown.style.display).toBe("block"));
  expect(toggle.getAttribute("aria-expanded")).toBe("true");

  root.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(dropdown.style.display).toBe("none");
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(toggle);
});

/// (partial: app command palette categories forwarding)
test("HumanApp.openCommandPalette forwards registered command categories into command palette", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ wire_major: 1, registry_version: "r1", roots: [], nodes: [] }), { headers: { "Content-Type": "application/json" } })));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  registerCommandCategory("app_addon_cat", { name: "Addon Actions", sequence: 10 });
  registerCommandProvider("app_addon_provider", {
    provide: () => [{ id: "cmd_addon", name: "Addon Action Item", category: "app_addon_cat" }]
  });

  try {
    app.openCommandPalette();
    const palette = getActiveCommandPalette();
    expect(palette).not.toBeNull();

    await vi.waitFor(() => {
      const items = palette!.node.querySelectorAll<HTMLElement>(".command-palette-item");
      expect(items.length).toBeGreaterThan(0);
      const target = Array.from(items).find(it => it.querySelector(".command-name")?.textContent === "Addon Action Item");
      expect(target).toBeDefined();
      expect(target?.querySelector(".command-category")?.textContent).toBe("Addon Actions");
    });

    palette?.close();
  } finally {
    commandCategories.remove("app_addon_cat");
    commandProviders.remove("app_addon_provider");
  }
});

/// (partial: the authorized menus are a built-in palette provider; hint instead of an empty message)
test("the command palette lists authorized menus, finds Notes from 'No', and hints before anything is typed", async () => {
  history.replaceState(null, "", "/erp");
  vi.stubGlobal("fetch", vi.fn(async (path: string) => new Response(JSON.stringify(path.endsWith("erp_load_web_menus")
    ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] } : []), { headers: { "Content-Type": "application/json" } })));
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  app.openCommandPalette();
  const palette = getActiveCommandPalette()!;
  const input = palette.node.querySelector<HTMLInputElement>(".command-palette-input")!;
  expect(input.placeholder).toBe("Search commands…");
  await vi.waitFor(() => expect(palette.node.querySelector(".command-name")?.textContent).toBe("Notes"));
  expect(palette.node.querySelector(".command-category")?.textContent).toBe("Menus");
  input.value = "no"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-name")).toHaveLength(1));
  input.value = "zzz"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(palette.node.querySelector<HTMLElement>(".command-palette-empty")?.textContent).toBe("No commands found"));
  palette.close();
});



/** An existing, archivable record in the default theme; `writes` collects every write-method call. */
async function startArchivableForm(): Promise<{ root: HTMLElement; writes: string[] }> {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  const writes: string[] = [];
  const note = description.fields["test.note"];
  const archivable = {
    ...description, fields: { "test.note": { ...note, active: { type: "boolean", label: "Active", required: false, readonly: false, relation: null, relation_field: null, selection: null } } },
    views: description.views.map(view => view.type !== "form" ? view : {
      ...view, nodes: [{ kind: "form", record_mode: "record", children: [1, 2] }, view.nodes[1],
        { kind: "field", force_save: false, field: { model: "test.note", name: "active" }, widget: "boolean_toggle", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Active" }]
    })
  };
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const method = path.split("/").pop()!;
    if (["copy", "unlink", "action_archive", "action_unarchive"].includes(method)) writes.push(method);
    const value = path.endsWith("get_res_users_settings") ? { homemenu_config: null } : path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 1, type: "form" }, { id: 2, type: "list" }], domain: null, context: null
    }, archivable) : path.endsWith("get_views") ? archivable
      : path.endsWith("web_read") ? [{ id: 5, name: "Existing note", active: true }]
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  await vi.waitFor(() => expect(root.querySelector(".form-actions-dropdown")).not.toBeNull());
  return { root, writes };
}

/// CYCLE3 MUST 1: after an unknown action outcome the record menu cannot send a write, in the default theme too.
test("an unknown action outcome disables the record menu writes and their jobs refuse", async () => {
  const { root, writes } = await startArchivableForm();
  (app as unknown as { uncertainMethod: boolean }).uncertainMethod = true;
  root.querySelector<HTMLButtonElement>(".form-reload-btn")!.click();
  const entries = (): HTMLButtonElement[] => ["duplicate", "archive", "delete"].map(name => root.querySelector<HTMLButtonElement>(`.form-${name}-btn`)!);
  await vi.waitFor(() => expect(entries().map(entry => entry.disabled)).toEqual([true, true, true]));
  expect(root.querySelector<HTMLButtonElement>(".form-reload-btn")!.disabled).toBe(false);
  // A stale enabled button must still not write.
  for (const entry of entries()) { entry.disabled = false; entry.click(); }
  await new Promise(done => setTimeout(done, 20));
  expect(writes).toEqual([]);
});

/// CYCLE3 SHOULD 5: the record menu is the shared dropdown (roles, aria-expanded, roving focus, Escape).
test("the record menu opens with the keyboard, roves focus and closes on Escape returning to the trigger", async () => {
  const { root } = await startArchivableForm();
  const toggle = root.querySelector<HTMLButtonElement>(".record-menu-btn")!;
  const menu = root.querySelector<HTMLElement>(".form-action-menu")!;
  expect(menu.getAttribute("role")).toBe("menu");
  expect([...menu.querySelectorAll("button")].map(item => item.getAttribute("role"))).toEqual(expect.arrayContaining(["menuitem"]));
  expect([...menu.querySelectorAll("button")].every(item => item.getAttribute("role") === "menuitem")).toBe(true);
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  toggle.focus();
  toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  expect(menu.classList.contains("open")).toBe(true);
  expect(root.querySelector<HTMLElement>(".form-duplicate-btn")!.hidden).toBe(false);
  await vi.waitFor(() => expect(document.activeElement).toBe(root.querySelector(".form-duplicate-btn")));
  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(root.querySelector(".form-archive-btn"));
  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  expect(menu.classList.contains("open")).toBe(false);
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(toggle);
});

/// CYCLE3 SHOULD 7: dispose releases the top-bar dropdown listeners; no document pointerdown listener outlives the app.
test("dispose releases the top-bar dropdown document listeners", async () => {
  history.replaceState(null, "", "/erp/action-20");
  const live = new Set<EventListenerOrEventListenerObject>();
  const add = document.addEventListener.bind(document), remove = document.removeEventListener.bind(document);
  vi.spyOn(document, "addEventListener").mockImplementation((type: string, listener: EventListenerOrEventListenerObject, options?: boolean | AddEventListenerOptions) => { if (type === "pointerdown") live.add(listener); add(type, listener, options); });
  vi.spyOn(document, "removeEventListener").mockImplementation((type: string, listener: EventListenerOrEventListenerObject, options?: boolean | EventListenerOptions) => { if (type === "pointerdown") live.delete(listener); remove(type, listener, options); });
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const value = path.endsWith("erp_load_web_menus") ? {
      wire_major: 1, registry_version: "r1", roots: [10],
      nodes: [
        { module: null, id: 10, parent_id: null, label: "Notes", action_id: null, children: [11], icon: null },
        { module: null, id: 11, parent_id: 10, label: "Folders", action_id: null, children: [12], icon: null },
        { module: null, id: 12, parent_id: 11, label: "Archive", action_id: 20, children: [], icon: null }
      ]
    } : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, {
      wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }], domain: null, context: null
    }, description) : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  expect(root.querySelector(".nav-dropdown-toggle")).not.toBeNull();
  expect(live.size).toBeGreaterThan(0);
  app.dispose(); app = undefined;
  expect(live.size).toBe(0);
});

/// CYCLE3 NIT 14: the addon context line survives a render that drops the title block it had moved under.
test("the context line returns to the sheet when a render leaves no title block", async () => {
  const { root } = await startArchivableForm();
  const line = root.querySelector<HTMLElement>(".content-sheet > .form-record-context")!;
  expect(line).not.toBeNull();
  const form = root.querySelector<HTMLElement>(".record-form")!;
  form.append(line);
  form.replaceChildren();
  await vi.waitFor(() => expect(root.querySelector(".content-sheet > .form-record-context")).toBe(line));
});

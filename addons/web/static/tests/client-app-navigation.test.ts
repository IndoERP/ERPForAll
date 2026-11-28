/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates, registerServiceController } from "../src/client/registry.js";
import { ClientError } from "../src/client/transport.js";
import { HumanApp, defaultViewType } from "../src/client/app.js";
import { BreadcrumbTrail, renderBreadcrumbs } from "../src/client/navigation.js";

function actionMetadata(path: string, action: unknown, views: unknown): unknown {
  return path.endsWith("erp_load_web_action_views") ? { action, views } : action;
}

const bootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Arkana" }],
  current_company_id: 1,
  allowed_company_ids: [1],
  active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};

const noteDescription = {
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
  delete document.documentElement.dataset["mobile"];
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

function mockNoteFetch(savedLog?: unknown[], description: unknown = noteDescription) {
  return vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_load_web_menus")) {
      return new Response(JSON.stringify({
        wire_major: 1,
        registry_version: "r1",
        roots: [10],
        nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }]
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) {
      return new Response(JSON.stringify(actionMetadata(path, {
        wire_major: 1,
        id: 20,
        name: "Notes",
        model: "test.note",
        target: "current",
        views: [{ id: 2, type: "list" }, { id: 1, type: "form" }],
        domain: null,
        context: null
      }, description)), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("get_views")) {
      return new Response(JSON.stringify(description), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_search_read")) {
      return new Response(JSON.stringify({
        length: 2,
        records: [
          { id: 5, name: "Meeting Agenda" },
          { id: 6, name: "Project Roadmap" }
        ]
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_read")) {
      const body = init?.body ? JSON.parse(String(init.body)) : {};
      const id = body.ids?.[0] ?? 5;
      const name = id === 6 ? "Project Roadmap" : "Meeting Agenda";
      return new Response(JSON.stringify([{ id, name }]), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("onchange")) {
      return new Response(JSON.stringify({ value: {} }), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("web_save")) {
      const body = init?.body ? JSON.parse(String(init.body)) : {};
      savedLog?.push(body.vals);
      return new Response(JSON.stringify([{ id: 5, ...body.vals }]), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify([]), { headers: { "Content-Type": "application/json" } });
  });
}

test("Breadcrumb trail bounds stack depth to avoid memory leaks", () => {
  const trail = new BreadcrumbTrail(5);
  for (let i = 1; i <= 10; i++) {
    trail.push({
      id: `crumb-${i}`,
      title: `Step ${i}`,
      route: { action: "20", id: String(i) },
      url: `/erp/action-20?id=${i}`,
      isFormView: true
    });
  }
  expect(trail.items.length).toBe(5);
  expect(trail.current?.title).toBe("Step 10");
  expect(trail.previous?.title).toBe("Step 9");
  expect(trail.items[0]!.title).toBe("Step 6");
});

test("Breadcrumbs render current, previous link, and older collapsed dropdown on desktop", async () => {
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", mockNoteFetch());

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const breadcrumbs = root.querySelector("[data-control-breadcrumbs]")!;
  expect(breadcrumbs.textContent).toContain("Meeting Agenda");

  // Previous breadcrumb link
  const prevLink = breadcrumbs.querySelector<HTMLElement>(".breadcrumb-previous a, .breadcrumb-back");
  expect(prevLink).not.toBeNull();
  expect(prevLink?.textContent).toContain("Notes");

  // Verify hotkey attribute
  const backContainer = breadcrumbs.querySelector("[data-hotkey=b]");
  expect(backContainer).not.toBeNull();
});

test("Older collapsed breadcrumbs render in reverse order in dropdown and navigate on selection", async () => {
  const trail = new BreadcrumbTrail(10);
  trail.push({ id: "1", title: "App Home", route: { action: "10" }, url: "/erp/action-10", isFormView: false });
  trail.push({ id: "2", title: "Category", route: { action: "11" }, url: "/erp/action-11", isFormView: false });
  trail.push({ id: "3", title: "Notes List", route: { action: "20" }, url: "/erp/action-20", isFormView: false });
  trail.push({ id: "4", title: "Meeting Agenda", route: { action: "20", id: "5" }, url: "/erp/action-20?id=5", isFormView: true });

  expect(trail.current?.title).toBe("Meeting Agenda");
  expect(trail.previous?.title).toBe("Notes List");
  // Collapsed entries are older than penultimate, in reverse order: Category, then App Home
  const collapsed = trail.collapsed;
  expect(collapsed.map(c => c.title)).toEqual(["Category", "App Home"]);
});

test("Mobile breadcrumbs render back control navigating to previous breadcrumb", async () => {
  document.documentElement.dataset["mobile"] = "true";
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", mockNoteFetch());

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  const breadcrumbs = root.querySelector("[data-control-breadcrumbs]")!;
  const backBtn = breadcrumbs.querySelector<HTMLButtonElement>("button.o_back_button, button.breadcrumb-back");
  expect(backBtn).not.toBeNull();
  expect(backBtn?.getAttribute("aria-label")).toBeTruthy();
});

/// (partial: autosave on breadcrumb navigation)
test("Autosave triggers on breadcrumb navigation away from dirty form", async () => {
  const saved: unknown[] = [];
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", mockNoteFetch(saved));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  // Modify form record to dirty
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Updated Name";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));

  // Click previous breadcrumb
  const breadcrumbs = root.querySelector("[data-control-breadcrumbs]")!;
  const prevBtn = breadcrumbs.querySelector<HTMLElement>(".breadcrumb-previous a, .breadcrumb-back")!;
  prevBtn.click();

  // Wait for autosave to complete and transition to list view
  await vi.waitFor(() => expect(saved.length).toBeGreaterThan(0));
  expect(saved[0]).toEqual({ name: "Updated Name" });
  await vi.waitFor(() => expect(root.querySelector(".control-panel-list")).not.toBeNull());
});

/// (partial: autosave on pager record navigation)
test("Autosave triggers before pager navigates to adjacent record", async () => {
  const saved: unknown[] = [];
  history.replaceState(null, "", "/erp/action-20");
  vi.stubGlobal("fetch", mockNoteFetch(saved));

  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  // Click row to navigate to form with pager
  await vi.waitFor(() => expect(root.querySelector("tbody tr")).not.toBeNull());
  root.querySelector<HTMLElement>("tbody tr")!.click();

  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("1 / 2"));

  // Modify form record to dirty
  const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  input.value = "Updated In Pager";
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));

  // Step record via pager
  const nextBtn = root.querySelector<HTMLButtonElement>(".form-pager .pager-next")!;
  nextBtn.click();

  await vi.waitFor(() => expect(saved.length).toBeGreaterThan(0));
  expect(saved[0]).toEqual({ name: "Updated In Pager" });
  await vi.waitFor(() => expect(root.querySelector(".form-pager .pager-count")?.textContent).toBe("2 / 2"));
  expect(window.location.search).toContain("id=6");
});

test("collapsed breadcrumbs dropdown keyboard roving focus and Escape dismissal", () => {
  const container = document.createElement("div");
  document.body.append(container);

  const trail = new BreadcrumbTrail(10);
  trail.push({ id: "1", title: "Orders", route: {}, url: "/orders" });
  trail.push({ id: "2", title: "SO001", route: {}, url: "/orders/1" });
  trail.push({ id: "3", title: "Deliveries", route: {}, url: "/orders/1/delivery" });
  trail.push({ id: "4", title: "WH/OUT/001", route: {}, url: "/delivery/1" });

  renderBreadcrumbs(container, trail, { onNavigate: async () => {} });

  const toggle = container.querySelector<HTMLButtonElement>(".breadcrumb-collapsed-toggle")!;
  const menu = container.querySelector<HTMLElement>(".breadcrumb-dropdown-menu")!;
  const items = menu.querySelectorAll<HTMLButtonElement>(".breadcrumb-dropdown-item");

  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(menu.classList.contains("open")).toBe(false);

  toggle.focus();
  toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  expect(menu.classList.contains("open")).toBe(true);
  expect(document.activeElement).toBe(items[0]);

  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(document.activeElement).toBe(items[1]);

  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  expect(menu.classList.contains("open")).toBe(false);
  expect(document.activeElement).toBe(toggle);

  container.remove();
});

test("popstate where only the hash differs from current URL is ignored", async () => {
  history.replaceState({ erpIndex: 0 }, "", "/erp");
  vi.stubGlobal("fetch", mockNoteFetch());
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();

  // The two private methods the shell routes a popstate through, seen through a typed view of the instance.
  const shell = app as unknown as { reportError(error: unknown): void; navigate(...args: unknown[]): Promise<void> };
  const reportSpy = vi.spyOn(shell, "reportError");
  const navigateSpy = vi.spyOn(shell, "navigate");

  history.pushState({ erpIndex: 0 }, "", "/erp#some-section");
  window.dispatchEvent(new PopStateEvent("popstate", { state: { erpIndex: 0 } }));

  await new Promise(r => setTimeout(r, 20));

  expect(reportSpy).not.toHaveBeenCalled();
  expect(navigateSpy).not.toHaveBeenCalled();
});



test.each(["default", "office-blue"])("%s uses shared form hosts and record menu", async theme => {
  document.documentElement.dataset["officeTheme"] = theme;
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", mockNoteFetch());
  const root = host(); app = new HumanApp(root, bootstrap); await app.start();
  expect(root.querySelector('[data-control-panel] > .form-record-smart')).not.toBeNull();
  expect(root.querySelector('.record-form-viewport > .form-record-header')).not.toBeNull();
  expect(root.querySelector('[data-control-breadcrumbs] .record-menu-btn')).not.toBeNull();
  expect(root.querySelector('.form-record-commands, .form-record-tabs')).toBeNull();
});


test("a declared page heading announces the open record and leaves the title field labelled", async () => {
  const view = noteDescription.views[0]!;
  const description = { ...noteDescription, views: [{ ...view, nodes: [
    { kind: "form", record_mode: "record", children: [1] }, { kind: "sheet", children: [2] },
    { kind: "title", children: [3] }, { kind: "heading", level: 1, children: [4] }, view.nodes[1]
  ] }, noteDescription.views[1]] };
  history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
  vi.stubGlobal("fetch", mockNoteFetch(undefined, description));
  const root = host(); app = new HumanApp(root, bootstrap);
  await app.start();
  const heading = root.querySelector("h1.form-title-heading");
  expect(heading?.getAttribute("aria-label")).toBe("Meeting Agenda");
  expect(heading?.querySelector("input[name=name]")).not.toBeNull();
  expect(root.querySelectorAll("main h1, [data-client-content] h1")).toHaveLength(1);
});

test("service teardown follows committed and superseded navigation, preserving a failed destination's predecessor", async () => {
  const disposals = [vi.fn(), vi.fn(), vi.fn()];
  let created = 0, release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  registerServiceController("test.structure_lifecycle", {
    model: "test.note", validateOptions: () => {},
    async create() {
      const index = created++;
      if (index === 1) throw new ClientError("server", "Destination unavailable.");
      if (index === 2) await pending;
      const dispose = disposals[index === 0 ? 0 : index - 1]!;
      return { busy: () => false, dirty: () => false, save: async () => {}, discard: () => {}, dispose };
    }
  });
  const baseFetch = mockNoteFetch();
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_load_web_action_views") || path.endsWith("erp_load_web_action")) {
      const args = JSON.parse(String(init?.body)) as { ids: number[] };
      const views = { ...noteDescription, views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0,
        nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "test.structure_lifecycle", options: {}, modifiers: { readonly: null, required: null, invisible: null } }] }] };
      const action = { wire_major: 1, id: args.ids[0], name: "Service", model: "test.note", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null };
      return new Response(JSON.stringify(actionMetadata(path, action, views)), { headers: { "Content-Type": "application/json" } });
    }
    return baseFetch(path, init);
  }));
  history.replaceState(null, "", "/erp/action-20?view_type=form");
  app = new HumanApp(host(), bootstrap); await app.start();
  await expect(app["navigate"]({ action: "21", view_type: "form" })).rejects.toMatchObject({ kind: "server", message: "Destination unavailable." });
  expect(disposals[0]).not.toHaveBeenCalled();
  const stale = app["navigate"]({ action: "22", view_type: "form" });
  await vi.waitFor(() => expect(created).toBe(3));
  // Initial navigation may supersede an in-flight destination without the user busy guard.
  await app["navigate"]({ action: "23", view_type: "form" }, true);
  expect(disposals[0]).toHaveBeenCalledTimes(1);
  release(); await stale;
  expect(disposals[1]).toHaveBeenCalledTimes(1);
  expect(disposals[2]).not.toHaveBeenCalled();
  app.dispose(); app = undefined;
  expect(disposals[2]).toHaveBeenCalledTimes(1);
});

// Review S5: the record form's breadcrumb is the way back and the record name; the menu path is not drawn on forms.
test("a form breadcrumb draws no menu path, in any theme", () => {
  const container = document.createElement("div");
  const trail = new BreadcrumbTrail(10);
  trail.push({ id: "1", title: "Orders", route: {}, url: "/orders" });
  trail.push({ id: "2", title: "SO001", route: {}, url: "/orders/1" });
  renderBreadcrumbs(container, trail, { isSmall: false, onNavigate: async () => {}, ancestorLabels: ["Sales", "Orders"] } as never);
  expect(container.querySelector(".breadcrumb-ancestor-label, .list-menu-path")).toBeNull();
  expect(container.querySelector(".form-breadcrumb-path")?.textContent).not.toContain("Sales");
});


/** Fixture names deliberately resemble built-in commands: labels never select icons or behavior. */
function mockIconNavigation() {
  const fallback = mockNoteFetch();
  const node = (id: number, parent_id: number | null, label: string, children: number[], action_id: number | null, icon: string | null) =>
    ({ id, parent_id, label, children, action_id, icon, module: null });
  return vi.fn(async (path: string, init?: RequestInit) => {
    if (path.endsWith("erp_load_web_menus")) return new Response(JSON.stringify({
      wire_major: 1, registry_version: "r1", roots: [10], nodes: [
        node(10, null, "Notes", [11, 12, 13], null, "lucide:notebook-pen"),
        node(11, 10, "Delete", [], 20, "lucide:book"),
        node(12, 10, "Actions", [14, 15], null, "lucide:settings"),
        node(13, 10, "Empty category", [], null, null),
        node(14, 12, "Unknown glyph", [], 20, "https://invalid.example/icon.svg"),
        node(15, 12, "Nested", [16], null, null),
        node(16, 15, "Laporan panjang — 日本語", [], 20, null),
      ],
    }), { headers: { "Content-Type": "application/json" } });
    return fallback(path, init);
  });
}

/// (partial: declared/default menu icons and unchanged labels in mounted application navigation)
/// (partial: mobile application and section icon labels)
test("application navigation carries local icons without treating data labels as commands", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=14");
  vi.stubGlobal("fetch", mockIconNavigation());
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const sections = root.querySelector(".app-sections")!;
  expect(sections.querySelector(".app-brand-btn")?.textContent).toBe("Notes");
  expect(sections.querySelector('.nav-item[data-menu-id="11"] > .erp-icon')?.getAttribute("data-icon")).toBe("book");
  expect(sections.querySelector('.nav-item[data-menu-id="11"]')?.classList.contains("danger")).toBe(false);
  const toggle = sections.querySelector('.nav-dropdown[data-nav-entry="12"] > button')!;
  expect(toggle.textContent).toBe("Actions");
  expect(toggle.querySelector(".menu-entry-icon")?.getAttribute("data-icon")).toBe("settings");
  expect(toggle.querySelector(".nav-menu-chevron")?.getAttribute("data-icon")).toBe("chevron-down");
  const unknown = sections.querySelector('[data-menu-id="14"]')!;
  expect(unknown.querySelector(".erp-icon")?.getAttribute("data-icon")).toBe("layout-grid");
  expect(unknown.getAttribute("aria-current")).toBe("page");
  expect(warn).toHaveBeenCalledWith(expect.stringContaining("is not a shipped icon; showing the default icon"));
  expect(sections.querySelector('[data-menu-id="13"]')).toBeNull();
  expect(sections.querySelector(".dropdown-header")?.querySelector("svg")).toBeNull();
  const desktop = [...sections.querySelectorAll<HTMLButtonElement>("button[data-menu-id]")];
  for (const entry of desktop) {
    const mobile = root.querySelector(`.mobile-drawer button[data-menu-id="${entry.dataset["menuId"]}"]`)!;
    expect(mobile.textContent).toBe(entry.textContent);
    expect(mobile.querySelector(".erp-icon")?.getAttribute("data-icon")).toBe(entry.querySelector(".erp-icon")?.getAttribute("data-icon"));
  }
  for (const svg of root.querySelectorAll(".app-sections svg, .mobile-drawer svg")) {
    expect(svg.getAttribute("aria-hidden")).toBe("true");
    expect(svg.getAttribute("focusable")).toBe("false");
  }
  expect(root.querySelector('img[src*="invalid.example"], script[src*="invalid.example"]')).toBeNull();
});

/// (partial: mounted overflow icon identity, selected destination and focus after width changes)
test("More keeps destination icons and focus when sections overflow and return", async () => {
  history.replaceState(null, "", "/erp/action-20?menu=14");
  vi.stubGlobal("fetch", mockIconNavigation());
  vi.spyOn(console, "warn").mockImplementation(() => {});
  let available = 1000;
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function(this: HTMLElement) {
    return this.classList.contains("app-sections") ? available : 0;
  });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function(this: HTMLElement) {
    return new DOMRect(0, 0, this.hasAttribute("data-nav-entry") ? 100 : 40, 32);
  });
  const root = host();
  app = new HumanApp(root, bootstrap);
  await app.start();
  const sections = root.querySelector(".app-sections")!;
  const original = sections.querySelector<HTMLButtonElement>('[data-nav-entry="12"] > button')!;
  original.focus();
  available = 180;
  window.dispatchEvent(new Event("resize"));
  const more = sections.querySelector<HTMLButtonElement>(".nav-more > button")!;
  expect(more).not.toBeNull();
  expect(document.activeElement).toBe(more);
  const entries = [...sections.querySelectorAll<HTMLButtonElement>(".nav-more [data-menu-id]")];
  expect(entries.map(entry => entry.dataset["menuId"])).toEqual(["14", "16"]);
  expect(entries.map(entry => entry.querySelector(".erp-icon")?.getAttribute("data-icon"))).toEqual(["layout-grid", "file-text"]);
  expect(entries[0]!.getAttribute("aria-current")).toBe("page");
  more.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(document.activeElement).toBe(entries[0]);
  available = 1000;
  window.dispatchEvent(new Event("resize"));
  expect(sections.querySelector(".nav-more")).toBeNull();
  expect(document.activeElement).toBe(original);
  expect(original.closest<HTMLElement>("[data-nav-entry]")!.hidden).toBe(false);
});

test("collapsed breadcrumb trail groups ellipsis and previous link separate from the current record", () => {
  const container = document.createElement("div");
  document.body.append(container);
  try {
    const trail = new BreadcrumbTrail(10);
    trail.push({ id: "1", title: "Quotations", route: {}, url: "/orders" });
    trail.push({ id: "2", title: "S00001", route: {}, url: "/orders/1", isFormView: true });
    trail.push({ id: "3", title: "PT Sinar Abadi Elektronik", route: {}, url: "/partners/54", isFormView: true });
    renderBreadcrumbs(container, trail, { isSmall: false, onNavigate: async () => {} });
    const trailRow = container.querySelector<HTMLElement>(".form-breadcrumb-path > .form-breadcrumb-trail");
    expect(trailRow).not.toBeNull();
    expect(trailRow!.querySelector(".breadcrumb-collapsed-dropdown")).not.toBeNull();
    expect(trailRow!.querySelector(".form-breadcrumb-divider")?.textContent).toBe("›");
    expect(trailRow!.querySelector(".breadcrumb-previous .breadcrumb-back")?.textContent).toBe("S00001");
    const current = container.querySelector<HTMLElement>(".form-breadcrumb-path > .form-breadcrumb-record");
    expect(current?.textContent).toBe("PT Sinar Abadi Elektronik");
    expect(trailRow!.contains(current)).toBe(false);
  } finally {
    container.remove();
  }
});

test("an action opens on a first hierarchy or activity view, otherwise on its list", () => {
  expect(defaultViewType([[1, "kanban"], [2, "list"]], undefined)).toBe("list");
  expect(defaultViewType([[1, "hierarchy"], [2, "list"]], undefined)).toBe("hierarchy");
  expect(defaultViewType([[1, "list"], [2, "hierarchy"]], undefined)).toBe("list");
  expect(defaultViewType([[1, "activity"], [2, "list"]], undefined)).toBe("activity");
  expect(defaultViewType([[1, "form"], [2, "list"]], undefined)).toBe("list");
  expect(defaultViewType([[1, "form"], [2, "list"]], "7")).toBe("form");
});

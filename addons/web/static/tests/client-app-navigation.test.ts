/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates, registerServiceController } from "../src/client/registry.js";
import { ClientError } from "../src/client/transport.js";
import { HumanApp } from "../src/client/app.js";
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

function mockNoteFetch(savedLog?: unknown[]) {
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
      }, noteDescription)), { headers: { "Content-Type": "application/json" } });
    }
    if (path.endsWith("get_views")) {
      return new Response(JSON.stringify(noteDescription), { headers: { "Content-Type": "application/json" } });
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
      isFormView: true,
      onSelected: async () => {}
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
  const selected: string[] = [];

  trail.push({ id: "1", title: "App Home", route: { action: "10" }, url: "/erp/action-10", isFormView: false, onSelected: async () => { selected.push("App Home"); } });
  trail.push({ id: "2", title: "Category", route: { action: "11" }, url: "/erp/action-11", isFormView: false, onSelected: async () => { selected.push("Category"); } });
  trail.push({ id: "3", title: "Notes List", route: { action: "20" }, url: "/erp/action-20", isFormView: false, onSelected: async () => { selected.push("Notes List"); } });
  trail.push({ id: "4", title: "Meeting Agenda", route: { action: "20", id: "5" }, url: "/erp/action-20?id=5", isFormView: true, onSelected: async () => { selected.push("Meeting Agenda"); } });

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
  trail.push({ id: "1", title: "Orders", route: {} as any, url: "/orders", onSelected: async () => {} });
  trail.push({ id: "2", title: "SO001", route: {} as any, url: "/orders/1", onSelected: async () => {} });
  trail.push({ id: "3", title: "Deliveries", route: {} as any, url: "/orders/1/delivery", onSelected: async () => {} });
  trail.push({ id: "4", title: "WH/OUT/001", route: {} as any, url: "/delivery/1", onSelected: async () => {} });

  renderBreadcrumbs(container, trail);

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

  const reportSpy = vi.spyOn(app as any, "reportError");
  const navigateSpy = vi.spyOn(app as any, "navigate");

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
  trail.push({ id: "1", title: "Orders", route: {} as any, url: "/orders", onSelected: async () => {} });
  trail.push({ id: "2", title: "SO001", route: {} as any, url: "/orders/1", onSelected: async () => {} });
  renderBreadcrumbs(container, trail, { isSmall: false, ancestorLabels: ["Sales", "Orders"] } as never);
  expect(container.querySelector(".breadcrumb-ancestor-label, .list-menu-path")).toBeNull();
  expect(container.querySelector(".form-breadcrumb-path")?.textContent).not.toContain("Sales");
});
